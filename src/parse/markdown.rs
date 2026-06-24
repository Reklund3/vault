use crate::parse::{ParseError, Parser, disambiguate_labels, estimate_tokens, sha256_hex};
use crate::store::Chunk;
use crate::types::Language;

/// Splits a markdown document into one chunk per level-2 (`##`) section. The
/// level-1 `#` title is treated as the document title, not a boundary — its
/// section (plus any intro prose before the first `##`) becomes a leading
/// "preamble" chunk only when it carries content beyond the bare title. Deeper
/// headings (`###`+) stay inside their parent `##` block. `##` markers inside
/// fenced code blocks are ignored so a `## step` shell comment never splits a
/// section.
///
/// Per the chunking table this applies to `convention`/`meta` markdown; `plan`
/// markdown is routed to the whole-file fallback upstream (see
/// [`crate::parse::select_parser`]).
pub struct MarkdownParser;

impl Parser for MarkdownParser {
    fn language(&self) -> Language {
        Language::Markdown
    }

    fn parse(&self, source: &str) -> Result<Vec<Chunk>, ParseError> {
        let lines: Vec<&str> = source.lines().collect();

        // Locate level-2 headings outside fenced code blocks.
        let mut fence: Option<char> = None;
        let mut boundaries: Vec<(usize, String)> = Vec::new();
        for (i, raw) in lines.iter().enumerate() {
            let t = raw.trim_start();
            if let Some(ch) = fence_char(t) {
                match fence {
                    None => fence = Some(ch),
                    Some(open) if open == ch => fence = None,
                    Some(_) => {}
                }
                continue;
            }
            if fence.is_some() {
                continue;
            }
            if let Some(text) = level2_heading(t) {
                boundaries.push((i, text));
            }
        }

        let mut chunks: Vec<Chunk> = Vec::new();
        let mut chunk_index: u32 = 0;

        let first = boundaries.first().map(|(i, _)| *i).unwrap_or(lines.len());
        if first > 0 && has_content_beyond_title(&lines[..first]) {
            let label = doc_title(&lines[..first]).unwrap_or_else(|| "(preamble)".to_string());
            let content = lines[..first].join("\n");
            push(&mut chunks, &mut chunk_index, label, content);
        }

        for (b, (start, heading)) in boundaries.iter().enumerate() {
            let end = boundaries
                .get(b + 1)
                .map(|(i, _)| *i)
                .unwrap_or(lines.len());
            let label = if heading.is_empty() {
                "(section)".to_string()
            } else {
                heading.clone()
            };
            let content = lines[*start..end].join("\n");
            push(&mut chunks, &mut chunk_index, label, content);
        }

        disambiguate_labels(&mut chunks);
        Ok(chunks)
    }
}

fn push(chunks: &mut Vec<Chunk>, chunk_index: &mut u32, label: String, content: String) {
    chunks.push(Chunk {
        language: Language::Markdown,
        content_hash: sha256_hex(content.as_bytes()),
        token_est: estimate_tokens(&content),
        label,
        content,
        chunk_index: *chunk_index,
    });
    *chunk_index += 1;
}

/// Returns the fence character (`` ` `` or `~`) if the line opens or closes a
/// fenced code block — i.e. starts with at least three of the same marker.
fn fence_char(trimmed: &str) -> Option<char> {
    if trimmed.starts_with("```") {
        Some('`')
    } else if trimmed.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

/// Returns the heading text if `trimmed` is exactly a level-2 ATX heading.
/// `# ` (title) and `### `+ (subsection) are rejected so only `##` splits.
fn level2_heading(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("## ")?;
    // strip_prefix already excludes `# ` and `### `: the former lacks the second
    // `#`, the latter has a `#` where the space is expected.
    Some(rest.trim().trim_end_matches('#').trim().to_string())
}

/// The level-1 `#` title text, if the preamble carries one.
fn doc_title(lines: &[&str]) -> Option<String> {
    for l in lines {
        if let Some(rest) = l.trim_start().strip_prefix("# ") {
            return Some(rest.trim().trim_end_matches('#').trim().to_string());
        }
    }
    None
}

/// True when the preamble has any non-blank line that is not solely the level-1
/// title — i.e. there is real intro content worth keeping as its own chunk.
fn has_content_beyond_title(lines: &[&str]) -> bool {
    lines.iter().any(|l| {
        let t = l.trim();
        !t.is_empty() && t.strip_prefix("# ").is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Chunk> {
        MarkdownParser.parse(src).expect("parse ok")
    }

    #[test]
    fn splits_on_level_two_headings() {
        let src = "\
# Title

Intro prose.

## First
alpha

## Second
beta
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 3); // preamble + 2 sections
        assert_eq!(chunks[0].label, "Title");
        assert!(chunks[0].content.contains("Intro prose."));
        assert_eq!(chunks[1].label, "First");
        assert!(chunks[1].content.contains("alpha"));
        assert_eq!(chunks[2].label, "Second");
        assert!(chunks[2].content.contains("beta"));
        assert_eq!(chunks[1].language, Language::Markdown);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].content_hash.len(), 64);
    }

    #[test]
    fn title_only_preamble_is_dropped() {
        let src = "\
# Title

## Only
body
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "Only");
    }

    #[test]
    fn subsections_stay_within_parent_block() {
        let src = "\
## Parent
lead

### Child
nested
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "Parent");
        assert!(chunks[0].content.contains("### Child"));
        assert!(chunks[0].content.contains("nested"));
    }

    #[test]
    fn ignores_headings_inside_code_fences() {
        let src = "\
## Real
```sh
## not a heading
echo hi
```
still real
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "Real");
        assert!(chunks[0].content.contains("## not a heading"));
    }

    #[test]
    fn closed_atx_heading_label_is_trimmed() {
        let src = "## Security ##\nbody\n";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "Security");
    }

    #[test]
    fn no_level_two_headings_yields_single_whole_chunk() {
        let src = "\
# Flat README

Just prose, no second-level headings.
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "Flat README");
        assert!(chunks[0].content.contains("Just prose"));
    }

    #[test]
    fn duplicate_headings_are_disambiguated() {
        let src = "\
## Notes
one

## Notes
two
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].label, "Notes");
        assert_eq!(chunks[1].label, "Notes#2");
    }

    #[test]
    fn empty_source_yields_no_chunks() {
        assert!(parse("").is_empty());
    }
}
