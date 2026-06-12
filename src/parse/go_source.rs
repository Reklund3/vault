use crate::parse::{
    ParseError, Parser, disambiguate_labels, doc_comment_start, estimate_tokens, sha256_hex,
};
use crate::store::Chunk;
use crate::types::Language;

pub struct GoParser;

impl Parser for GoParser {
    fn language(&self) -> Language {
        Language::Go
    }

    fn parse(&self, source: &str) -> Result<Vec<Chunk>, ParseError> {
        let lines: Vec<&str> = source.lines().collect();
        let mut scanner = LineScanner::default();
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut open: Option<OpenDef> = None;
        let mut chunk_index: u32 = 0;

        for (i, raw_line) in lines.iter().enumerate() {
            let inside_block_comment_at_start = scanner.in_block_comment;
            let inside_raw_string_at_start = scanner.in_raw_string;
            let net = scanner.scan_line(raw_line, i)?;

            if !inside_block_comment_at_start
                && !inside_raw_string_at_start
                && open.is_none()
                && scanner.group_depth == 0
                && let Some(name) = parse_decl_header(raw_line)
            {
                open = Some(OpenDef {
                    name,
                    start_line: i,
                });
            }

            scanner.group_depth += net;
            if scanner.group_depth < 0 {
                return Err(ParseError::Unterminated {
                    line: open.as_ref().map(|d| d.start_line + 1).unwrap_or(i + 1),
                });
            }

            // A declaration is complete only when every delimiter group has
            // closed *and* we are not suspended inside a multi-line raw string
            // or block comment (both of which leave `group_depth` untouched).
            if let Some(def) = &open
                && scanner.group_depth == 0
                && !scanner.in_raw_string
                && !scanner.in_block_comment
            {
                let (label, emit) = match &def.name {
                    DeclName::Named { label, exported } => (label.clone(), *exported),
                    DeclName::Method { label, exported } => (label.clone(), *exported),
                    // Grouped `const (`/`var (`/`type (` blocks are emitted
                    // whole, regardless of whether every member is exported.
                    // This honors the literal "const/var blocks" boundary in
                    // the plan; flipping to strict exported-only is a one-line
                    // predicate change here if ever required.
                    DeclName::Block { keyword } => {
                        let ident = first_block_ident(&lines[def.start_line..=i])
                            .unwrap_or_else(|| "block".to_string());
                        (format!("{keyword} {ident}"), true)
                    }
                };

                if emit {
                    let doc_start = doc_comment_start(&lines, def.start_line);
                    let content = lines[doc_start..=i].join("\n");
                    let content_hash = sha256_hex(content.as_bytes());
                    let token_est = estimate_tokens(&content);
                    chunks.push(Chunk {
                        language: Language::Go,
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
    name: DeclName,
    start_line: usize,
}

/// The shape of a recognized top-level declaration. `Named`/`Method` carry a
/// fully-rendered label and the export decision; `Block` defers its label to
/// emit time because the representative identifier lives inside the group body.
enum DeclName {
    Named { label: String, exported: bool },
    Method { label: String, exported: bool },
    Block { keyword: &'static str },
}

#[derive(Default)]
struct LineScanner {
    /// Combined nesting over `{`/`(`/`[`. Go delimits func/struct/interface
    /// bodies with braces but const/var/type/import groups and multi-line
    /// signatures with parens, and generics with brackets — a single decl is
    /// complete only when all of them unwind. Well-formed Go never crosses
    /// delimiter kinds (`{ )`), so one combined counter is sufficient.
    group_depth: i32,
    in_block_comment: bool,
    block_comment_start: Option<usize>,
    /// Go raw strings (backtick-delimited) can span lines, so this is cross-line
    /// state like `in_block_comment`. Raw strings have no escapes.
    in_raw_string: bool,
}

impl LineScanner {
    /// Returns the net opener-minus-closer count on this line, ignoring anything
    /// inside comments, interpreted strings, raw strings, and rune literals.
    /// Updates cross-line state (`in_block_comment`, `in_raw_string`) in place.
    fn scan_line(&mut self, line: &str, line_idx: usize) -> Result<i32, ParseError> {
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut net: i32 = 0;
        let mut in_interp = false;
        let mut in_rune = false;
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

            if self.in_raw_string {
                if b == b'`' {
                    self.in_raw_string = false;
                }
                i += 1;
                continue;
            }

            if in_line_comment {
                break;
            }

            if in_interp {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    in_interp = false;
                }
                i += 1;
                continue;
            }

            if in_rune {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'\'' {
                    in_rune = false;
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
                b'"' => in_interp = true,
                b'`' => self.in_raw_string = true,
                b'\'' => in_rune = true,
                b'{' | b'(' | b'[' => net += 1,
                b'}' | b')' | b']' => net -= 1,
                _ => {}
            }
            i += 1;
        }

        Ok(net)
    }
}

/// Recognize a top-level declaration at column 0. Returns `None` for any line
/// that does not begin a `func`/`type`/`const`/`var` declaration (package,
/// import, statements inside bodies, etc.).
fn parse_decl_header(line: &str) -> Option<DeclName> {
    if let Some(rest) = strip_keyword(line, "func") {
        return Some(parse_func(rest));
    }
    for keyword in ["type", "const", "var"] {
        if let Some(rest) = strip_keyword(line, keyword) {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                return Some(DeclName::Block { keyword });
            }
            let name = read_ident(rest);
            if name.is_empty() {
                return None;
            }
            let exported = is_exported(&name);
            return Some(DeclName::Named {
                label: format!("{keyword} {name}"),
                exported,
            });
        }
    }
    None
}

fn parse_func(rest: &str) -> DeclName {
    let rest = rest.trim_start();
    if rest.starts_with('(') {
        return parse_method(rest);
    }
    let name = read_ident(rest);
    if name.is_empty() {
        return DeclName::Named {
            label: "func".to_string(),
            exported: false,
        };
    }
    let exported = is_exported(&name);
    DeclName::Named {
        label: format!("func {name}"),
        exported,
    }
}

/// Parse `(recv) Method...` into a `func Type.Method` label. The receiver's
/// closing paren is the first `)` (receiver types never contain parens), and
/// the type collapses pointer (`*T`) and generic (`T[P]`) forms to the base
/// type name.
fn parse_method(rest: &str) -> DeclName {
    let Some(close) = rest.find(')') else {
        return DeclName::Named {
            label: "func".to_string(),
            exported: false,
        };
    };
    let recv = &rest[1..close];
    let after = rest[close + 1..].trim_start();
    let method = read_ident(after);
    if method.is_empty() {
        return DeclName::Named {
            label: "func".to_string(),
            exported: false,
        };
    }
    let exported = is_exported(&method);
    let label = match receiver_type(recv) {
        Some(ty) => format!("func {ty}.{method}"),
        None => format!("func {method}"),
    };
    DeclName::Method { label, exported }
}

/// Extract the base receiver type from a receiver clause body, e.g.
/// `s *Stack[T]` → `Stack`, `Buffer` → `Buffer`, `*os.File` → `os.File`.
fn receiver_type(recv: &str) -> Option<String> {
    let last = recv.split_whitespace().last()?;
    let last = last.trim_start_matches('*');
    let base = match last.find('[') {
        Some(idx) => &last[..idx],
        None => last,
    };
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// First identifier inside a grouped `const (`/`var (`/`type (` block, used as
/// the block's representative label. Skips the opener up to `(`, blank lines,
/// and line/block-comment lines.
fn first_block_ident(block_lines: &[&str]) -> Option<String> {
    let mut seen_open = false;
    for line in block_lines {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }
        if !seen_open {
            let pos = line.find('(')?;
            seen_open = true;
            let name = read_ident(&line[pos + 1..]);
            if !name.is_empty() {
                return Some(name);
            }
            continue;
        }
        if trimmed.starts_with(')') {
            continue;
        }
        let name = read_ident(trimmed);
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// A declaration keyword must start at column 0 and be followed by whitespace.
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

fn read_ident(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Go visibility rule: an identifier is exported iff its first character is an
/// uppercase letter (Unicode-aware). Leading `_` or lowercase means unexported.
fn is_exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Chunk> {
        GoParser.parse(src).expect("parse ok")
    }

    fn labels(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn parses_exported_func() {
        let src = "\
func Build(req Request) error {
    return nil
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "func Build");
        assert_eq!(chunks[0].language, Language::Go);
        assert!(chunks[0].content.contains("return nil"));
        assert_eq!(chunks[0].content_hash.len(), 64);
        assert!(chunks[0].token_est > 0);
    }

    #[test]
    fn skips_unexported_func() {
        let src = "\
func helper() int {
    return 1
}
";
        assert!(parse(src).is_empty());
    }

    #[test]
    fn attaches_leading_doc_comment() {
        let src = "\
// Build runs the build.
// It returns an error on failure.
func Build() error {
    return nil
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("// Build runs the build."));
        assert!(chunks[0].content.contains("// It returns an error"));
    }

    #[test]
    fn blank_line_breaks_doc_attachment() {
        let src = "\
// Detached comment.

func Build() error {
    return nil
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].content.contains("Detached"));
    }

    #[test]
    fn struct_type_is_whole_chunk() {
        let src = "\
type Point struct {
    X int
    Y int
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "type Point");
        assert!(chunks[0].content.contains("Y int"));
    }

    #[test]
    fn interface_is_whole_chunk() {
        let src = "\
type Builder interface {
    Build() error
    Close() error
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "type Builder");
        assert!(chunks[0].content.contains("Close() error"));
    }

    #[test]
    fn single_line_type_and_const() {
        let src = "\
type Celsius float64
const Pi = 3.14159
var Logger = newLogger()
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["type Celsius", "const Pi", "var Logger"]);
    }

    #[test]
    fn bodyless_func_is_single_line_chunk() {
        // Assembly-implemented stub: no `{ }` body.
        let src = "\
func Sqrt(x float64) float64
func After() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["func Sqrt", "func After"]);
        assert!(!chunks[0].content.contains("func After"));
    }

    #[test]
    fn multi_line_signature() {
        let src = "\
func Build(
    a int,
    b string,
) (Result, error) {
    return Result{}, nil
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "func Build");
        assert!(chunks[0].content.contains("b string"));
        assert!(chunks[0].content.contains("return Result{}, nil"));
    }

    #[test]
    fn value_method_label() {
        let src = "\
func (s Server) Build() error {
    return nil
}
";
        let chunks = parse(src);
        assert_eq!(chunks[0].label, "func Server.Build");
    }

    #[test]
    fn pointer_method_label() {
        let src = "\
func (s *Server) Build() error {
    return nil
}
";
        let chunks = parse(src);
        assert_eq!(chunks[0].label, "func Server.Build");
    }

    #[test]
    fn generic_method_label_strips_type_params() {
        let src = "\
func (s *Stack[T]) Push(x T) {
    s.items = append(s.items, x)
}
";
        let chunks = parse(src);
        assert_eq!(chunks[0].label, "func Stack.Push");
    }

    #[test]
    fn unexported_method_skipped() {
        let src = "\
func (s *Server) start() {
    s.running = true
}
";
        assert!(parse(src).is_empty());
    }

    #[test]
    fn const_block_emitted_whole_with_first_ident_label() {
        let src = "\
const (
    StatusOK = 200
    StatusNotFound = 404
)
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "const StatusOK");
        assert!(chunks[0].content.contains("StatusNotFound = 404"));
    }

    #[test]
    fn var_block_with_leading_comment_label_skips_comment() {
        let src = "\
var (
    // ErrClosed is returned on a closed handle.
    ErrClosed = errors.New(\"closed\")
)
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "var ErrClosed");
    }

    #[test]
    fn method_without_receiver_variable_label() {
        let src = "\
func (Server) Start() {
}
";
        let chunks = parse(src);
        assert_eq!(chunks[0].label, "func Server.Start");
    }

    #[test]
    fn doc_comment_attaches_to_const_block() {
        let src = "\
// Codes groups the HTTP status constants.
const (
    StatusOK = 200
)
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "const StatusOK");
        assert!(chunks[0].content.starts_with("// Codes groups"));
    }

    #[test]
    fn block_label_known_limitation_with_multiline_comment() {
        // Known limitation: `first_block_ident` skips `//` and single-line `/*`
        // comment lines, but a free-form multi-line block comment whose interior
        // lines are not `*`-prefixed leaks its first word into the block label.
        // This is label-only — the chunk content is still complete and correct —
        // and the input style is rare in gofmt'd Go, so we accept it rather than
        // grow comment-state tracking into a label helper.
        let src = "\
const (
    /*
    Description without a leading star.
    */
    Foo = 1
)
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "const Description");
        assert!(chunks[0].content.contains("Foo = 1"));
    }

    #[test]
    fn rune_literal_with_brace_does_not_unbalance() {
        let src = "\
func Scan() rune {
    c := '{'
    return c
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "func Scan");
    }

    #[test]
    fn struct_tag_raw_string_is_ignored() {
        let src = "\
type User struct {
    Name string `json:\"name\"`
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "type User");
    }

    #[test]
    fn raw_string_spanning_lines_with_brace_inside() {
        let src = "\
const Schema = `
CREATE TABLE t (
    id INT
}`
type After int
";
        let chunks = parse(src);
        // The raw string holds an unbalanced `}` and `(`; neither must leak into
        // delimiter tracking, and `type After` after it must still be found.
        assert_eq!(labels(&chunks), ["const Schema", "type After"]);
        assert!(chunks[0].content.contains("CREATE TABLE"));
    }

    #[test]
    fn import_block_emits_nothing_and_does_not_swallow_next_decl() {
        let src = "\
package main

import (
    \"fmt\"
    \"os\"
)

func Run() {
    fmt.Println(os.Args)
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["func Run"]);
    }

    #[test]
    fn braces_in_strings_and_block_comments_ignored() {
        let src = "\
func Conf() string {
    /* a } brace { in a comment */
    return \"a } brace { in a string\"
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "func Conf");
    }

    #[test]
    fn nested_func_literal_stays_with_parent() {
        let src = "\
func Outer() {
    f := func() int { return 1 }
    _ = f
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "func Outer");
    }

    #[test]
    fn duplicate_labels_are_disambiguated() {
        let src = "\
func Build() {
}

func Build() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["func Build", "func Build#2"]);
    }

    #[test]
    fn unterminated_func_errors() {
        let src = "\
func NeverClosed() {
    x := 1
";
        let err = GoParser.parse(src).expect_err("should fail");
        match err {
            ParseError::Unterminated { line } => assert_eq!(line, 1),
            other => panic!("expected Unterminated, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_block_comment_errors() {
        let src = "\
/* never closed
func X() {}
";
        let err = GoParser.parse(src).expect_err("should fail");
        assert!(matches!(err, ParseError::UnterminatedBlockComment { .. }));
    }

    #[test]
    fn registry_recognizes_go_extension() {
        let parser = super::super::parser_for("go").expect("registered");
        assert_eq!(parser.language(), Language::Go);
    }

    #[test]
    fn registry_is_extension_case_insensitive() {
        assert!(super::super::parser_for("GO").is_some());
    }
}
