mod go_source;
mod proto;
mod rust_source;

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::store::Chunk;
use crate::types::Language;

pub use go_source::GoParser;
pub use proto::ProtoParser;
pub use rust_source::RustParser;

pub trait Parser {
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
}

/// Look up a parser by file extension (no leading dot).
pub fn parser_for(extension: &str) -> Option<Box<dyn Parser>> {
    match extension.to_ascii_lowercase().as_str() {
        "proto" => Some(Box::new(ProtoParser)),
        "go" => Some(Box::new(GoParser)),
        "rs" => Some(Box::new(RustParser)),
        _ => None,
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
    ((chars + 3) / 4) as u32
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
