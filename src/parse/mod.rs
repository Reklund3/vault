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
