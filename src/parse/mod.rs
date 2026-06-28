mod go_source;
mod markdown;
mod openapi;
mod proto;
mod rust_source;

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::store::Chunk;
use crate::types::{DocType, Language};

pub use go_source::GoParser;
pub use markdown::MarkdownParser;
pub use openapi::OpenApiParser;
pub use proto::ProtoParser;
pub use rust_source::RustParser;

pub trait Parser {
    #[allow(dead_code)]
    fn language(&self) -> Language;
    fn parse(&self, source: &str) -> Result<Vec<Chunk>, ParseError>;
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unterminated definition starting at line {line}: missing closing brace")]
    Unterminated { line: usize },

    #[error("unterminated block comment starting at line {line}")]
    UnterminatedBlockComment { line: usize },

    #[error("malformed definition header at line {line}: {detail}")]
    MalformedHeader { line: usize, detail: String },

    #[error("structural parse error: {detail}")]
    Structural { detail: String },
}

/// Look up a parser by file extension (no leading dot). Extension is an
/// unambiguous signal only for these source languages — `select_parser` is the
/// classification-aware entry point used by the indexer.
pub fn parser_for(extension: &str) -> Option<Box<dyn Parser>> {
    match extension.to_ascii_lowercase().as_str() {
        "proto" => Some(Box::new(ProtoParser)),
        "go" => Some(Box::new(GoParser)),
        "rs" => Some(Box::new(RustParser)),
        _ => None,
    }
}

/// Choose the structural parser for a classified file, or `None` to emit a
/// single whole-file chunk. Keyed on `(doc_type, language)` because the right
/// boundary depends on both axes (see the chunking table in CLAUDE.md):
///
/// - `plan` is always whole-file, whatever the language.
/// - OpenAPI dispatches off the *classified language*, not the extension —
///   `.yaml`/`.yml`/`.json` are shared with non-spec files, and the classifier
///   is what tells a spec apart.
/// - Markdown splits per `##` block here; a `plan` markdown file is already
///   handled by the whole-file rule above, so only `convention`/`meta` reach
///   the parser.
/// - proto/go/rust map straight from language; `extension` is the final
///   fallback for when the classifier returned `Unknown`.
pub fn select_parser(
    doc_type: DocType,
    language: Language,
    extension: &str,
) -> Option<Box<dyn Parser>> {
    if doc_type == DocType::Plan {
        return None;
    }
    match language {
        Language::Proto => Some(Box::new(ProtoParser)),
        Language::Go => Some(Box::new(GoParser)),
        Language::Rust => Some(Box::new(RustParser)),
        Language::OpenApi => Some(Box::new(OpenApiParser)),
        Language::Markdown => Some(Box::new(MarkdownParser)),
        _ => parser_for(extension),
    }
}

/// Token ceiling for a single fallback chunk. Whole-file fallback (`plan` docs
/// and any file no structural parser claims) is split into windows no larger
/// than this so a big file can't blow past the embedder's input limit and abort
/// the whole document (the failure mode behind finding 5B). Kept well under
/// nomic-embed-text-v1.5's 8192-token context — the char/4 `estimate_tokens`
/// underestimates dense code, so the margin absorbs that slack — and small
/// enough to keep retrieval granularity reasonable.
pub(crate) const MAX_FALLBACK_CHUNK_TOKENS: u32 = 1500;

/// Split whole-file fallback content into ordered, embeddable windows.
///
/// Returns `(chunks, oversize_lines_truncated)`. Behavior:
/// - Content already under [`MAX_FALLBACK_CHUNK_TOKENS`] yields exactly one
///   chunk labeled `filename` at `chunk_index` 0 — byte-identical to the prior
///   single-chunk fallback, so small files are unaffected.
/// - Larger content is greedily packed by whole lines (terminators preserved
///   via `split_inclusive`, so concatenating the chunks reproduces the source)
///   until the next line would exceed the ceiling, then a new window starts.
/// - A single line longer than the ceiling (minified JSON/JS, a one-line log,
///   a base64 blob) can't be line-packed. It's **truncated** to the ceiling,
///   not char-split into many windows: truncation keeps the head intact for the
///   downstream per-chunk secret scan and never stores the tail, so a secret
///   can't be bisected across two windows where neither half trips the scan.
///   The lost tail is a single dense blob with near-zero retrieval value; each
///   such line is counted so the sync report can surface it.
///
/// Labels are disambiguated (`filename`, `filename#2`, …) to satisfy
/// `UNIQUE(document_id, label)` — the structural parsers do this inside
/// `parse()`, but the fallback path never ran a parser, so it must do it here.
pub(crate) fn whole_file_chunks(
    content: &str,
    language: Language,
    filename: &str,
) -> (Vec<Chunk>, usize) {
    let make = |body: &str, idx: u32| Chunk {
        language,
        label: filename.to_string(),
        content: body.to_string(),
        content_hash: sha256_hex(body.as_bytes()),
        token_est: estimate_tokens(body),
        chunk_index: idx,
    };

    // Fast path: fits in one chunk → identical to the historical fallback.
    if estimate_tokens(content) <= MAX_FALLBACK_CHUNK_TOKENS {
        return (vec![make(content, 0)], 0);
    }

    let max_chars = MAX_FALLBACK_CHUNK_TOKENS as usize * 4;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut window = String::new();
    let mut window_chars = 0usize;
    let mut idx: u32 = 0;
    let mut truncated_lines = 0usize;

    let flush =
        |window: &mut String, window_chars: &mut usize, idx: &mut u32, chunks: &mut Vec<Chunk>| {
            if !window.is_empty() {
                chunks.push(make(window, *idx));
                *idx += 1;
                window.clear();
                *window_chars = 0;
            }
        };

    for line in content.split_inclusive('\n') {
        let line_chars = line.chars().count();

        // Pathological single line over the ceiling: flush what we have, then
        // emit the truncated head as its own chunk. char_indices keeps the cut
        // on a UTF-8 boundary.
        if line_chars > max_chars {
            flush(&mut window, &mut window_chars, &mut idx, &mut chunks);
            let head = match line.char_indices().nth(max_chars) {
                Some((byte_idx, _)) => &line[..byte_idx],
                None => line,
            };
            chunks.push(make(head, idx));
            idx += 1;
            truncated_lines += 1;
            continue;
        }

        // Adding this line would overflow the current window → seal it first.
        if window_chars > 0 && window_chars + line_chars > max_chars {
            flush(&mut window, &mut window_chars, &mut idx, &mut chunks);
        }
        window.push_str(line);
        window_chars += line_chars;
    }
    flush(&mut window, &mut window_chars, &mut idx, &mut chunks);

    disambiguate_labels(&mut chunks);
    (chunks, truncated_lines)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Rough token estimate — 4 chars/token is the conventional ballpark for
/// English + code. The hook's real budget runs through tiktoken later; this is
/// good enough for chunk-level filtering inside the store.
pub(crate) fn estimate_tokens(content: &str) -> u32 {
    let chars = content.chars().count();
    chars.div_ceil(4) as u32
}

/// Enforce `UNIQUE(document_id, label)` by suffixing duplicates `#2`, `#3`, …
/// in source order. The first occurrence keeps its bare label.
pub(crate) fn disambiguate_labels(chunks: &mut [Chunk]) {
    let mut seen: HashMap<String, u32> = HashMap::new();
    for chunk in chunks.iter_mut() {
        let count = seen.entry(chunk.label.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            chunk.label = format!("{}#{}", chunk.label, count);
        }
    }
}

/// Walk backward from `def_line` collecting contiguous `//` doc-comment lines.
/// Stops at the first blank or non-comment line. Returns the index of the first
/// line of the chunk (the doc comment start, or `def_line` itself when no doc
/// comment precedes it). Shared by the proto and Go parsers — both use `//`
/// line comments with identical attachment rules.
pub(crate) fn doc_comment_start(lines: &[&str], def_line: usize) -> usize {
    let mut start = def_line;
    while start > 0 {
        let candidate = lines[start - 1].trim_start();
        if candidate.starts_with("//") {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_always_whole_file() {
        // A plan doc gets one whole-file chunk regardless of language.
        assert!(select_parser(DocType::Plan, Language::Markdown, "md").is_none());
        assert!(select_parser(DocType::Plan, Language::Proto, "proto").is_none());
    }

    #[test]
    fn markdown_splits_only_outside_plan() {
        let p = select_parser(DocType::Convention, Language::Markdown, "md").expect("parser");
        assert_eq!(p.language(), Language::Markdown);
        let p = select_parser(DocType::Meta, Language::Markdown, "md").expect("parser");
        assert_eq!(p.language(), Language::Markdown);
    }

    #[test]
    fn openapi_dispatches_off_language_not_extension() {
        // `.yaml`/`.json` carry no parser by extension, but the classified
        // language selects the OpenAPI parser.
        assert!(parser_for("yaml").is_none());
        let p = select_parser(DocType::Contract, Language::OpenApi, "yaml").expect("parser");
        assert_eq!(p.language(), Language::OpenApi);
        let p = select_parser(DocType::Contract, Language::OpenApi, "json").expect("parser");
        assert_eq!(p.language(), Language::OpenApi);
    }

    #[test]
    fn source_languages_map_straight_through() {
        assert_eq!(
            select_parser(DocType::Contract, Language::Proto, "proto")
                .unwrap()
                .language(),
            Language::Proto
        );
        assert_eq!(
            select_parser(DocType::Convention, Language::Go, "go")
                .unwrap()
                .language(),
            Language::Go
        );
        assert_eq!(
            select_parser(DocType::Convention, Language::Rust, "rs")
                .unwrap()
                .language(),
            Language::Rust
        );
    }

    #[test]
    fn small_file_is_one_whole_chunk() {
        // Under the ceiling → identical to the historical single-chunk fallback:
        // one chunk, bare filename label, chunk_index 0, hash of the full body.
        let body = "fn main() {}\n";
        let (chunks, truncated) = whole_file_chunks(body, Language::Rust, "main.rs");
        assert_eq!(chunks.len(), 1);
        assert_eq!(truncated, 0);
        assert_eq!(chunks[0].label, "main.rs");
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].content, body);
        assert_eq!(chunks[0].content_hash, sha256_hex(body.as_bytes()));
    }

    #[test]
    fn large_file_windows_into_ordered_chunks() {
        // ~100 chars/line × 400 lines ≈ 40k chars ≈ 10k tokens → several windows.
        let line = format!("{}\n", "x".repeat(98));
        let body: String = line.repeat(400);
        let (chunks, truncated) = whole_file_chunks(&body, Language::Unknown, "big.txt");

        assert!(chunks.len() > 1, "expected multiple windows");
        assert_eq!(truncated, 0);

        // chunk_index is monotonic from 0.
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.chunk_index, i as u32);
            // Every window fits under the ceiling (the whole point).
            assert!(
                c.token_est <= MAX_FALLBACK_CHUNK_TOKENS,
                "chunk {i} token_est {} exceeds ceiling",
                c.token_est
            );
            // Per-window hash, not the file's.
            assert_eq!(c.content_hash, sha256_hex(c.content.as_bytes()));
        }

        // Labels disambiguated: first bare, rest suffixed.
        assert_eq!(chunks[0].label, "big.txt");
        assert_eq!(chunks[1].label, "big.txt#2");

        // Line-aligned packing is lossless: concatenation reproduces the source.
        let reassembled: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert_eq!(reassembled, body);
    }

    #[test]
    fn oversize_single_line_is_truncated_head_only() {
        // One line, no newline, well over the char ceiling → can't be line-packed.
        let ceiling_chars = MAX_FALLBACK_CHUNK_TOKENS as usize * 4;
        let line = "a".repeat(ceiling_chars + 5000);
        let (chunks, truncated) = whole_file_chunks(&line, Language::Unknown, "blob.min.js");

        assert_eq!(truncated, 1);
        assert_eq!(chunks.len(), 1);
        // Head kept, tail dropped — shorter than the original, within the ceiling.
        assert!(chunks[0].content.chars().count() <= ceiling_chars);
        assert!(chunks[0].content.len() < line.len());
        assert!(chunks[0].token_est <= MAX_FALLBACK_CHUNK_TOKENS);
    }

    #[test]
    fn unknown_language_falls_back_to_extension() {
        // Classifier returned Unknown, but a known extension still resolves.
        assert_eq!(
            select_parser(DocType::Convention, Language::Unknown, "go")
                .unwrap()
                .language(),
            Language::Go
        );
        // No parser by language or extension → whole-file fallback.
        assert!(select_parser(DocType::Convention, Language::Unknown, "txt").is_none());
    }
}
