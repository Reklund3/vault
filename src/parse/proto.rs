use crate::parse::{
    ParseError, Parser, disambiguate_labels, doc_comment_start, estimate_tokens, sha256_hex,
};
use crate::store::Chunk;
use crate::types::Language;

pub struct ProtoParser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefKind {
    Message,
    Service,
    Enum,
}

impl DefKind {
    fn as_str(self) -> &'static str {
        match self {
            DefKind::Message => "message",
            DefKind::Service => "service",
            DefKind::Enum => "enum",
        }
    }

    /// Top-level constructs that we consume but do not emit as chunks. They
    /// carry braces, so we still need to track their depth to avoid swallowing
    /// the next real definition.
    fn skipped_from(line: &str) -> Option<&'static str> {
        for kw in ["option", "extend"] {
            if line.starts_with(kw) {
                let rest = &line[kw.len()..];
                if rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
                    return Some(kw);
                }
            }
        }
        None
    }
}

impl Parser for ProtoParser {
    fn language(&self) -> Language {
        Language::Proto
    }

    fn parse(&self, source: &str) -> Result<Vec<Chunk>, ParseError> {
        let lines: Vec<&str> = source.lines().collect();
        let mut scanner = LineScanner::default();
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut open: Option<OpenDef> = None;
        let mut chunk_index: u32 = 0;

        for (i, raw_line) in lines.iter().enumerate() {
            let inside_block_comment_at_start = scanner.in_block_comment;
            let net = scanner.scan_line(raw_line, i)?;

            if !inside_block_comment_at_start && open.is_none() && scanner.brace_depth == 0 {
                if let Some(header) = parse_def_header(raw_line, i)? {
                    open = Some(OpenDef {
                        kind: header.kind,
                        name: header.name,
                        start_line: i,
                    });
                } else if DefKind::skipped_from(raw_line).is_some() {
                    open = Some(OpenDef {
                        kind: DefKind::Message, // sentinel — will not be emitted
                        name: String::new(),
                        start_line: i,
                    });
                }
            }

            scanner.brace_depth += net;
            if scanner.brace_depth < 0 {
                return Err(ParseError::Unterminated {
                    line: open.as_ref().map(|d| d.start_line + 1).unwrap_or(i + 1),
                });
            }

            if let Some(def) = &open {
                if scanner.brace_depth == 0 {
                    if !def.name.is_empty() {
                        let doc_start = doc_comment_start(&lines, def.start_line);
                        let content = lines[doc_start..=i].join("\n");
                        let label = format!("{} {}", def.kind.as_str(), def.name);
                        let content_hash = sha256_hex(content.as_bytes());
                        let token_est = estimate_tokens(&content);
                        chunks.push(Chunk {
                            language: Language::Proto,
                            label,
                            content,
                            content_hash,
                            token_est,
                            chunk_index,
                        });
                        chunk_index += 1;
                    }
                    open = None;
                }
            }
        }

        if scanner.in_block_comment {
            return Err(ParseError::UnterminatedBlockComment {
                line: scanner.block_comment_start.unwrap_or(0) + 1,
            });
        }
        if let Some(def) = open {
            return Err(ParseError::Unterminated {
                line: def.start_line + 1,
            });
        }

        disambiguate_labels(&mut chunks);
        Ok(chunks)
    }
}

struct OpenDef {
    kind: DefKind,
    name: String,
    start_line: usize,
}

#[derive(Default)]
struct LineScanner {
    brace_depth: i32,
    in_block_comment: bool,
    block_comment_start: Option<usize>,
}

impl LineScanner {
    /// Returns the net `{` minus `}` count on this line, ignoring characters
    /// inside line comments, block comments, and string literals. Updates
    /// cross-line state (`in_block_comment`) in place.
    fn scan_line(&mut self, line: &str, line_idx: usize) -> Result<i32, ParseError> {
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut net: i32 = 0;
        let mut in_string = false;
        let mut in_line_comment = false;

        while i < bytes.len() {
            let b = bytes[i];

            if self.in_block_comment {
                if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    self.in_block_comment = false;
                    self.block_comment_start = None;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }

            if in_line_comment {
                break;
            }

            if in_string {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    in_string = false;
                }
                i += 1;
                continue;
            }

            if b == b'/' && i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'/' => {
                        in_line_comment = true;
                        i += 2;
                        continue;
                    }
                    b'*' => {
                        self.in_block_comment = true;
                        self.block_comment_start = Some(line_idx);
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }

            match b {
                b'"' => in_string = true,
                b'{' => net += 1,
                b'}' => net -= 1,
                _ => {}
            }
            i += 1;
        }

        Ok(net)
    }
}

struct ParsedHeader {
    kind: DefKind,
    name: String,
}

/// Parse a top-level definition header at column 0. Returns `Ok(None)` if the
/// line is not a definition. Returns an error if the keyword is present but
/// the identifier is missing/malformed.
fn parse_def_header(line: &str, line_idx: usize) -> Result<Option<ParsedHeader>, ParseError> {
    let (kind, rest) = if let Some(rest) = strip_keyword(line, "message") {
        (DefKind::Message, rest)
    } else if let Some(rest) = strip_keyword(line, "service") {
        (DefKind::Service, rest)
    } else if let Some(rest) = strip_keyword(line, "enum") {
        (DefKind::Enum, rest)
    } else {
        return Ok(None);
    };

    let rest = rest.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();

    let first = name.chars().next();
    let valid_first = matches!(first, Some(c) if c.is_alphabetic() || c == '_');
    if name.is_empty() || !valid_first {
        return Err(ParseError::MalformedHeader {
            line: line_idx + 1,
            detail: format!("{} keyword not followed by an identifier", kind.as_str()),
        });
    }

    Ok(Some(ParsedHeader { kind, name }))
}

/// A definition header must start at column 0 (no leading whitespace) and the
/// keyword must be followed by whitespace.
fn strip_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    if !line.starts_with(keyword) {
        return None;
    }
    let rest = &line[keyword.len()..];
    if rest.starts_with(|c: char| c.is_whitespace()) {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Chunk> {
        ProtoParser.parse(src).expect("parse ok")
    }

    #[test]
    fn parses_single_message() {
        let src = "\
message BuildRequest {
  string id = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message BuildRequest");
        assert_eq!(chunks[0].language, Language::Proto);
        assert!(chunks[0].content.contains("string id = 1;"));
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].content_hash.len(), 64);
        assert!(chunks[0].token_est > 0);
    }

    #[test]
    fn includes_leading_doc_comment() {
        let src = "\
// BuildRequest carries the auth token.
// Sent on every protobuf call.
message BuildRequest {
  string token = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("// BuildRequest carries"));
        assert!(
            chunks[0]
                .content
                .contains("// Sent on every protobuf call.")
        );
    }

    #[test]
    fn skips_doc_comment_separated_by_blank_line() {
        let src = "\
// Not attached to anything.

message BuildRequest {
  string token = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].content.contains("Not attached"));
    }

    #[test]
    fn parses_service_and_enum() {
        let src = "\
service BuildService {
  rpc Build (BuildRequest) returns (BuildResponse);
}

enum Status {
  STATUS_UNKNOWN = 0;
  STATUS_OK = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].label, "service BuildService");
        assert_eq!(chunks[1].label, "enum Status");
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[1].chunk_index, 1);
    }

    #[test]
    fn nested_messages_stay_with_parent() {
        let src = "\
message Outer {
  message Inner {
    string x = 1;
  }
  Inner inner = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message Outer");
        assert!(chunks[0].content.contains("message Inner"));
    }

    #[test]
    fn single_line_message() {
        let src = "message Empty {}\n";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message Empty");
    }

    #[test]
    fn ignores_braces_in_strings() {
        let src = "\
message Conf {
  string template = 1 [default = \"{ ignored }\"];
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message Conf");
    }

    #[test]
    fn ignores_braces_in_block_comments() {
        let src = "\
message Conf {
  /* block comment with } and { inside */
  string id = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message Conf");
    }

    #[test]
    fn block_comment_can_hide_a_fake_def_header() {
        let src = "\
/*
message FakeMessage {
}
*/
message RealMessage {
  string x = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message RealMessage");
    }

    #[test]
    fn syntax_and_imports_alone_produce_no_chunks() {
        let src = "\
syntax = \"proto3\";
package olympus.build.v1;
import \"google/protobuf/timestamp.proto\";
";
        let chunks = parse(src);
        assert!(chunks.is_empty());
    }

    #[test]
    fn top_level_option_with_braces_does_not_swallow_next_def() {
        let src = "\
option (custom_opt) = {
  nested: \"value\"
};

message AfterOption {
  string x = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message AfterOption");
    }

    #[test]
    fn extend_block_does_not_swallow_next_def() {
        let src = "\
extend google.protobuf.MessageOptions {
  optional string my_opt = 50000;
}

message AfterExtend {
  string x = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "message AfterExtend");
    }

    #[test]
    fn unterminated_definition_errors() {
        let src = "\
message NeverClosed {
  string id = 1;
";
        let err = ProtoParser.parse(src).expect_err("should fail");
        match err {
            ParseError::Unterminated { line } => assert_eq!(line, 1),
            other => panic!("expected Unterminated, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_block_comment_errors() {
        let src = "\
/* never closed
message X {}
";
        let err = ProtoParser.parse(src).expect_err("should fail");
        assert!(matches!(err, ParseError::UnterminatedBlockComment { .. }));
    }

    #[test]
    fn cross_kind_collision_keeps_distinct_labels() {
        // `message Foo` and `enum Foo` at the same scope — the rendering label
        // includes the kind, so they don't actually collide.
        let src = "\
message Foo {
  string x = 1;
}

enum Foo {
  FOO_UNKNOWN = 0;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].label, "message Foo");
        assert_eq!(chunks[1].label, "enum Foo");
    }

    #[test]
    fn same_kind_same_name_is_disambiguated_with_suffix() {
        // Not valid proto, but the parser must guarantee UNIQUE(document_id,
        // label) regardless of the input. Suffix the second occurrence.
        let src = "\
message Dup {
  string a = 1;
}

message Dup {
  string b = 1;
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].label, "message Dup");
        assert_eq!(chunks[1].label, "message Dup#2");
    }

    #[test]
    fn registry_recognizes_proto_extension() {
        let parser = super::super::parser_for("proto").expect("registered");
        assert_eq!(parser.language(), Language::Proto);
    }

    #[test]
    fn registry_is_extension_case_insensitive() {
        assert!(super::super::parser_for("PROTO").is_some());
    }
}
