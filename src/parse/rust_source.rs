use crate::parse::{ParseError, Parser, disambiguate_labels, estimate_tokens, sha256_hex};
use crate::store::Chunk;
use crate::types::Language;

pub struct RustParser;

impl Parser for RustParser {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn parse(&self, source: &str) -> Result<Vec<Chunk>, ParseError> {
        let lines: Vec<&str> = source.lines().collect();
        let mut scanner = LineScanner::default();
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut chunk_index: u32 = 0;
        let mut mode = Mode::Module;
        let mut open: Option<OpenItem> = None;
        // First line of the contiguous run of doc comments + attributes that
        // should be folded into the next item's chunk. Reset by blank lines and
        // any non-preamble, non-item line.
        let mut preamble_start: Option<usize> = None;
        // Accumulates a multi-line `impl ... {` header until its body brace opens.
        let mut pending_impl: Option<String> = None;

        for (i, raw_line) in lines.iter().enumerate() {
            let pre_depth = scanner.group_depth;
            let pre_in_block_comment = scanner.block_comment_depth > 0;
            let pre_in_string = scanner.in_string_like();
            let net = scanner.scan_line(raw_line, i)?;

            // --- multi-line impl header accumulation ---
            if let Some(text) = pending_impl.as_mut() {
                text.push(' ');
                text.push_str(raw_line);
                scanner.group_depth += net;
                if scanner.group_depth < 0 {
                    return Err(ParseError::Unterminated { line: i + 1 });
                }
                if scanner.group_depth > 0 {
                    let (ty, trait_impl) = parse_impl_target(pending_impl.take().unwrap().as_str());
                    mode = Mode::Impl {
                        ty,
                        trait_impl,
                        body_depth: scanner.group_depth,
                    };
                    preamble_start = None;
                } else if raw_line.contains('{') {
                    pending_impl = None;
                    preamble_start = None;
                }
                continue;
            }

            let at_detection_level = open.is_none()
                && !pre_in_block_comment
                && !pre_in_string
                && match &mode {
                    Mode::Module => pre_depth == 0,
                    Mode::Impl { body_depth, .. } => pre_depth == *body_depth,
                };

            // Staged so we never assign `mode` while the match below borrows it.
            let mut next_mode: Option<Mode> = None;

            if at_detection_level {
                let trimmed = raw_line.trim_start();
                if trimmed.is_empty() {
                    preamble_start = None;
                } else if is_preamble_line(trimmed) {
                    if preamble_start.is_none() {
                        preamble_start = Some(i);
                    }
                } else {
                    match &mode {
                        Mode::Module => {
                            if is_impl_header(trimmed) {
                                let new_depth = pre_depth + net;
                                if new_depth > 0 {
                                    let (ty, trait_impl) = parse_impl_target(raw_line);
                                    next_mode = Some(Mode::Impl {
                                        ty,
                                        trait_impl,
                                        body_depth: new_depth,
                                    });
                                } else if !raw_line.contains('{') {
                                    // Header continues on the next line(s).
                                    pending_impl = Some(raw_line.to_string());
                                }
                                // else: empty `impl T {}` on one line — nothing to do.
                                preamble_start = None;
                            } else if let Some(item) = parse_pub_item(raw_line) {
                                let start = preamble_start.take().unwrap_or(i);
                                if let ItemKind::Mod = item.kind {
                                    // `pub mod` is shallow: emit the declaration only,
                                    // never the inline body, and let depth tracking
                                    // skip the body so inner items aren't indexed.
                                    // Truncating at the first `{` assumes rustfmt-clean
                                    // input, the same contract as proto's column-0 rule.
                                    let decl = match raw_line.find('{') {
                                        Some(b) => &raw_line[..b],
                                        None => raw_line,
                                    };
                                    let mut content = String::new();
                                    for line in &lines[start..i] {
                                        content.push_str(line);
                                        content.push('\n');
                                    }
                                    content.push_str(decl.trim_end());
                                    push_chunk(
                                        &mut chunks,
                                        &mut chunk_index,
                                        format!("mod {}", item.name),
                                        content,
                                    );
                                } else {
                                    open = Some(OpenItem {
                                        label: format!("{} {}", item.kind.keyword(), item.name),
                                        start_line: start,
                                        emit: true,
                                        close_depth: 0,
                                    });
                                }
                            } else {
                                preamble_start = None;
                            }
                        }
                        Mode::Impl {
                            ty,
                            trait_impl,
                            body_depth,
                        } => {
                            if let Some(method) = parse_method_header(raw_line) {
                                let emit = *trait_impl || method.is_pub;
                                let start = preamble_start.take().unwrap_or(i);
                                open = Some(OpenItem {
                                    label: format!("{ty}::{}", method.name),
                                    start_line: start,
                                    emit,
                                    close_depth: *body_depth,
                                });
                            } else {
                                preamble_start = None;
                            }
                        }
                    }
                }
            }

            if let Some(m) = next_mode {
                mode = m;
            }

            scanner.group_depth += net;
            if scanner.group_depth < 0 {
                return Err(ParseError::Unterminated {
                    line: open.as_ref().map(|o| o.start_line + 1).unwrap_or(i + 1),
                });
            }

            if let Some(item) = &open
                && scanner.group_depth == item.close_depth
                && scanner.block_comment_depth == 0
                && !scanner.in_string_like()
            {
                if item.emit {
                    let content = lines[item.start_line..=i].join("\n");
                    push_chunk(&mut chunks, &mut chunk_index, item.label.clone(), content);
                }
                open = None;
            }

            let exit_impl =
                matches!(&mode, Mode::Impl { body_depth, .. } if scanner.group_depth < *body_depth);
            if exit_impl {
                mode = Mode::Module;
            }
        }

        if scanner.block_comment_depth > 0 {
            return Err(ParseError::UnterminatedBlockComment {
                line: scanner.block_comment_start.unwrap_or(0) + 1,
            });
        }
        if let Some(item) = open {
            return Err(ParseError::Unterminated {
                line: item.start_line + 1,
            });
        }

        disambiguate_labels(&mut chunks);
        Ok(chunks)
    }
}

fn push_chunk(chunks: &mut Vec<Chunk>, chunk_index: &mut u32, label: String, content: String) {
    let content_hash = sha256_hex(content.as_bytes());
    let token_est = estimate_tokens(&content);
    chunks.push(Chunk {
        language: Language::Rust,
        label,
        content,
        content_hash,
        token_est,
        chunk_index: *chunk_index,
    });
    *chunk_index += 1;
}

enum Mode {
    Module,
    Impl {
        ty: String,
        trait_impl: bool,
        body_depth: i32,
    },
}

struct OpenItem {
    label: String,
    start_line: usize,
    emit: bool,
    /// The `group_depth` this item returns to when its body closes: 0 for a
    /// top-level item, the impl's body depth for a method.
    close_depth: i32,
}

#[derive(Clone, Copy)]
enum ItemKind {
    Fn,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Mod,
}

impl ItemKind {
    fn keyword(self) -> &'static str {
        match self {
            ItemKind::Fn => "fn",
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
            ItemKind::Trait => "trait",
            ItemKind::TypeAlias => "type",
            ItemKind::Const => "const",
            ItemKind::Mod => "mod",
        }
    }
}

struct PubItem {
    kind: ItemKind,
    name: String,
}

struct Method {
    name: String,
    is_pub: bool,
}

#[derive(Default)]
struct LineScanner {
    /// Combined nesting over `{`/`(`/`[`. Angle brackets are deliberately not
    /// counted — `<`/`>` are ambiguous with comparison, shift, and `->`/`=>`.
    group_depth: i32,
    /// Rust block comments nest, so this is a depth, not a flag.
    block_comment_depth: u32,
    block_comment_start: Option<usize>,
    /// Regular `"..."` strings — cross-line, since Rust string literals may span
    /// lines.
    in_string: bool,
    /// Raw string `r#"…"#` — cross-line; holds the `#` count needed to close.
    in_raw_string: Option<usize>,
}

impl LineScanner {
    fn in_string_like(&self) -> bool {
        self.in_string || self.in_raw_string.is_some()
    }

    /// Net opener-minus-closer count on this line, ignoring comments, strings,
    /// raw strings, char literals, and lifetimes. Updates cross-line state.
    fn scan_line(&mut self, line: &str, line_idx: usize) -> Result<i32, ParseError> {
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut net: i32 = 0;
        let mut in_line_comment = false;
        let mut in_char = false;
        let mut prev_ident = false;

        while i < bytes.len() {
            let b = bytes[i];

            if self.block_comment_depth > 0 {
                if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    self.block_comment_depth += 1;
                    i += 2;
                    continue;
                }
                if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    self.block_comment_depth -= 1;
                    if self.block_comment_depth == 0 {
                        self.block_comment_start = None;
                    }
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }

            if let Some(hashes) = self.in_raw_string {
                if b == b'"' {
                    let mut j = i + 1;
                    let mut count = 0;
                    while j < bytes.len() && bytes[j] == b'#' && count < hashes {
                        j += 1;
                        count += 1;
                    }
                    if count == hashes {
                        self.in_raw_string = None;
                        i = j;
                        prev_ident = false;
                        continue;
                    }
                }
                i += 1;
                continue;
            }

            if self.in_string {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    self.in_string = false;
                }
                i += 1;
                prev_ident = false;
                continue;
            }

            if in_line_comment {
                break;
            }

            if in_char {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'\'' {
                    in_char = false;
                }
                i += 1;
                prev_ident = false;
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
                        if self.block_comment_depth == 0 {
                            self.block_comment_start = Some(line_idx);
                        }
                        self.block_comment_depth += 1;
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }

            // Raw (byte) string: r"…", r#"…"#, br"…", br#"…"#. Only when the
            // `r`/`b` does not continue a preceding identifier.
            if (b == b'r' || b == b'b')
                && !prev_ident
                && let Some((consumed, hashes)) = raw_string_prefix(bytes, i)
            {
                self.in_raw_string = Some(hashes);
                i += consumed;
                prev_ident = false;
                continue;
            }

            if b == b'"' {
                self.in_string = true;
                i += 1;
                prev_ident = false;
                continue;
            }

            if b == b'\'' {
                if is_char_literal_start(bytes, i) {
                    in_char = true;
                    i += 1;
                } else {
                    // A lifetime (`'a`) or label — consume the tick and the name;
                    // neither contains delimiters we count.
                    i += 1;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                }
                prev_ident = false;
                continue;
            }

            match b {
                b'{' | b'(' | b'[' => {
                    net += 1;
                    prev_ident = false;
                }
                b'}' | b')' | b']' => {
                    net -= 1;
                    prev_ident = false;
                }
                _ => {
                    prev_ident = b.is_ascii_alphanumeric() || b == b'_';
                }
            }
            i += 1;
        }

        Ok(net)
    }
}

/// If a raw (byte) string opens at `i`, returns `(bytes consumed through the
/// opening quote, hash count)`. Matches `r"`, `r#…"`, `br"`, `br#…"`.
fn raw_string_prefix(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if bytes[j] == b'b' {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'r' {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while j < bytes.len() && bytes[j] == b'#' {
        j += 1;
        hashes += 1;
    }
    if j < bytes.len() && bytes[j] == b'"' {
        Some((j + 1 - i, hashes))
    } else {
        None
    }
}

/// Decide whether a `'` at `i` opens a char literal (vs a lifetime/label). A
/// char literal is `'\…` (any escape, including `'\u{7b}'`) or `'x'` (a single
/// unit followed immediately by the closing tick).
fn is_char_literal_start(bytes: &[u8], i: usize) -> bool {
    if i + 1 >= bytes.len() {
        return false;
    }
    if bytes[i + 1] == b'\\' {
        return true;
    }
    i + 2 < bytes.len() && bytes[i + 2] == b'\''
}

fn is_preamble_line(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("/*")
}

fn is_impl_header(trimmed: &str) -> bool {
    let t = strip_leading_words(trimmed, &["unsafe", "default"]);
    match t.strip_prefix("impl") {
        Some(rest) => rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == '<'),
        None => false,
    }
}

/// Parse the implementing type and whether this is a trait impl from an impl
/// header (single line or accumulated multi-line). `impl<T> Tr<T> for Foo<T>`
/// → (`Foo`, true); `impl Foo` → (`Foo`, false).
fn parse_impl_target(text: &str) -> (String, bool) {
    let head = match text.find('{') {
        Some(b) => &text[..b],
        None => text,
    };
    let head = strip_leading_words(head.trim_start(), &["unsafe", "default"]);
    let head = head.strip_prefix("impl").unwrap_or(head);
    let head = skip_angle_generics(head);
    let head = match split_on_word(head, "where") {
        Some((before, _)) => before,
        None => head,
    };
    match split_on_word(head, "for") {
        Some((_, target)) => (type_name_from(target), true),
        None => (type_name_from(head), false),
    }
}

fn parse_pub_item(line: &str) -> Option<PubItem> {
    let t = line.trim_start();
    let (is_pub, t) = strip_visibility(t);
    if !is_pub {
        return None;
    }
    let t = strip_modifiers(t);
    let kinds = [
        ("fn", ItemKind::Fn),
        ("struct", ItemKind::Struct),
        ("enum", ItemKind::Enum),
        ("trait", ItemKind::Trait),
        ("type", ItemKind::TypeAlias),
        ("const", ItemKind::Const),
        ("mod", ItemKind::Mod),
    ];
    for (kw, kind) in kinds {
        if let Some(rest) = strip_word(t, kw) {
            let name = read_name(rest);
            if name.is_empty() {
                return None;
            }
            return Some(PubItem { kind, name });
        }
    }
    None
}

fn parse_method_header(line: &str) -> Option<Method> {
    let t = line.trim_start();
    let (is_pub, t) = strip_visibility(t);
    let t = strip_modifiers(t);
    let rest = strip_word(t, "fn")?;
    let name = read_name(rest);
    if name.is_empty() {
        return None;
    }
    Some(Method { name, is_pub })
}

/// Strip a leading `pub` / `pub(crate)` / `pub(in path)` visibility. Returns
/// whether the item is public and the remainder positioned at the next token.
fn strip_visibility(t: &str) -> (bool, &str) {
    match t.strip_prefix("pub") {
        Some(rest) if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) => {
            (true, rest.trim_start())
        }
        Some(rest) if rest.starts_with('(') => match rest.find(')') {
            Some(close) => (true, rest[close + 1..].trim_start()),
            None => (true, rest[1..].trim_start()),
        },
        _ => (false, t),
    }
}

/// Consume leading item modifiers, leaving the remainder at the item keyword.
/// `const` is consumed only when it is the `const fn` modifier, not the `const`
/// item keyword. `extern` consumes an optional ABI string (`extern "C"`).
fn strip_modifiers(mut t: &str) -> &str {
    loop {
        t = t.trim_start();
        let mut matched = false;
        for kw in ["default", "async", "unsafe"] {
            if let Some(rest) = strip_word(t, kw) {
                t = rest;
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if let Some(rest) = strip_word(t, "extern") {
            let rest = rest.trim_start();
            if let Some(after_quote) = rest.strip_prefix('"')
                && let Some(close) = after_quote.find('"')
            {
                t = &after_quote[close + 1..];
                continue;
            }
            t = rest;
            continue;
        }
        if let Some(rest) = strip_word(t, "const") {
            if strip_word(rest.trim_start(), "fn").is_some() {
                t = rest;
                continue;
            }
            // `const` is the item keyword here.
            break;
        }
        break;
    }
    t
}

fn strip_word<'a>(t: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = t.strip_prefix(keyword)?;
    if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
        Some(rest)
    } else {
        None
    }
}

fn strip_leading_words<'a>(mut s: &'a str, words: &[&str]) -> &'a str {
    loop {
        s = s.trim_start();
        let mut matched = false;
        for w in words {
            if let Some(rest) = strip_word(s, w) {
                s = rest;
                matched = true;
                break;
            }
        }
        if !matched {
            return s;
        }
    }
}

fn read_name(rest: &str) -> String {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("r#").unwrap_or(rest);
    rest.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

fn skip_angle_generics(s: &str) -> &str {
    let s = s.trim_start();
    if !s.starts_with('<') {
        return s;
    }
    let mut depth = 0i32;
    for (idx, ch) in s.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return s[idx + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    s
}

fn type_name_from(target: &str) -> String {
    let path: String = target
        .trim()
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '<' && *c != '{')
        .collect();
    path.rsplit("::").next().unwrap_or(&path).to_string()
}

/// Find `word` as a whole word and split around it, returning `(before, after)`.
fn split_on_word<'a>(s: &'a str, word: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let mut start = 0;
    while let Some(rel) = s[start..].find(word) {
        let idx = start + rel;
        let after = idx + word.len();
        let before_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some((&s[..idx], &s[after..]));
        }
        start = after;
    }
    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Chunk> {
        RustParser.parse(src).expect("parse ok")
    }

    fn labels(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn parses_pub_fn() {
        let src = "\
pub fn build(req: Request) -> Result<(), Error> {
    Ok(())
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "fn build");
        assert_eq!(chunks[0].language, Language::Rust);
        assert!(chunks[0].content.contains("Ok(())"));
        assert_eq!(chunks[0].content_hash.len(), 64);
    }

    #[test]
    fn skips_private_fn() {
        let src = "\
fn helper() -> i32 {
    1
}
";
        assert!(parse(src).is_empty());
    }

    #[test]
    fn pub_crate_is_public() {
        let src = "\
pub(crate) fn internal_api() {
    todo!()
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn internal_api"]);
    }

    #[test]
    fn struct_enum_trait_type_const() {
        let src = "\
pub struct Point {
    x: i32,
}

pub enum Color {
    Red,
}

pub trait Draw {
    fn draw(&self);
}

pub type Id = u64;
pub const MAX: usize = 100;
";
        let chunks = parse(src);
        assert_eq!(
            labels(&chunks),
            [
                "struct Point",
                "enum Color",
                "trait Draw",
                "type Id",
                "const MAX"
            ]
        );
    }

    #[test]
    fn unit_and_tuple_structs_single_line() {
        let src = "\
pub struct Unit;
pub struct Pair(pub i32, pub i32);
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["struct Unit", "struct Pair"]);
    }

    #[test]
    fn trait_with_default_method_is_one_chunk() {
        let src = "\
pub trait Greeter {
    fn name(&self) -> String;
    fn greet(&self) -> String {
        format!(\"hi {}\", self.name())
    }
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "trait Greeter");
        assert!(chunks[0].content.contains("fn greet"));
    }

    #[test]
    fn doc_comment_and_attribute_preamble_attach() {
        let src = "\
/// A point in 2D space.
#[derive(Debug, Clone)]
pub struct Point {
    x: i32,
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("/// A point in 2D space."));
        assert!(chunks[0].content.contains("#[derive(Debug, Clone)]"));
    }

    #[test]
    fn multi_line_attribute_preamble_attaches() {
        let src = "\
#[derive(
    Debug,
    Clone,
)]
pub struct Wide {
    x: i32,
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "struct Wide");
        assert!(chunks[0].content.starts_with("#[derive("));
        assert!(chunks[0].content.contains("Clone,"));
    }

    #[test]
    fn blank_line_breaks_preamble() {
        let src = "\
/// Detached.

pub struct Point {
    x: i32,
}
";
        let chunks = parse(src);
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].content.contains("Detached"));
    }

    #[test]
    fn inherent_impl_emits_only_pub_methods() {
        let src = "\
pub struct Server;

impl Server {
    pub fn build(&self) -> i32 {
        self.helper()
    }

    fn helper(&self) -> i32 {
        1
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["struct Server", "Server::build"]);
    }

    #[test]
    fn trait_impl_emits_all_methods() {
        let src = "\
pub struct Server;

impl std::fmt::Display for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, \"server\")
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["struct Server", "Server::fmt"]);
    }

    #[test]
    fn generic_impl_strips_type_params() {
        let src = "\
impl<T> Stack<T> {
    pub fn push(&mut self, x: T) {
        self.items.push(x)
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["Stack::push"]);
    }

    #[test]
    fn generic_trait_impl_labels_by_type() {
        let src = "\
impl<T: Clone> Container<T> for Stack<T> {
    fn get(&self) -> T {
        self.items[0].clone()
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["Stack::get"]);
    }

    #[test]
    fn char_escape_with_braces_does_not_unbalance() {
        let src = "\
pub fn f() {
    let _: char = '\\u{7b}';
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn f"]);
    }

    #[test]
    fn char_brace_literal_does_not_unbalance() {
        let src = "\
pub fn f() -> char {
    '{'
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn f"]);
    }

    #[test]
    fn lifetime_and_char_in_same_signature() {
        let src = "\
pub fn first<'a>(x: &'a str) -> char {
    'x'
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn first"]);
    }

    #[test]
    fn hash_matched_raw_string_with_quote_and_hash_inside() {
        let src = "\
pub const Q: &str = r#\"has a \" and a # inside\"#;
pub const AFTER: u8 = 1;
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["const Q", "const AFTER"]);
    }

    #[test]
    fn raw_string_with_unbalanced_brace() {
        let src = "\
pub const T: &str = r#\"fn fake() { unbalanced\"#;
pub fn real() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["const T", "fn real"]);
    }

    #[test]
    fn raw_identifier_is_not_a_raw_string() {
        // The scanner must not enter raw-string mode for `r#try` (no `"` after
        // the `#`). The label drops the `r#` and reads `try`; the raw-ness is a
        // lexical escape for the keyword, not part of the human-facing name, so
        // `fn try` is the intended label.
        let src = "\
pub fn r#try() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn try"]);
    }

    #[test]
    fn nested_block_comments() {
        let src = "\
/* outer /* inner */ still outer */
pub fn f() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn f"]);
    }

    #[test]
    fn const_fn_vs_const_item() {
        let src = "\
pub const fn compute() -> u8 {
    1
}
pub const LIMIT: u8 = 5;
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn compute", "const LIMIT"]);
    }

    #[test]
    fn extern_abi_fn() {
        let src = "\
pub extern \"C\" fn callback() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn callback"]);
    }

    #[test]
    fn async_unsafe_fn_modifiers() {
        let src = "\
pub async unsafe fn risky() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn risky"]);
    }

    #[test]
    fn pub_mod_declaration_is_chunked() {
        let src = "pub mod config;\n";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["mod config"]);
        assert_eq!(chunks[0].content, "pub mod config;");
    }

    #[test]
    fn pub_mod_inline_body_is_shallow() {
        let src = "\
pub mod api {
    pub fn inner() {
    }
}
";
        let chunks = parse(src);
        // The module declaration is chunked; its inner items are NOT.
        assert_eq!(labels(&chunks), ["mod api"]);
        assert!(!chunks[0].content.contains("inner"));
    }

    #[test]
    fn multi_line_string_does_not_break_brace_tracking() {
        let src = "\
pub fn f() {
    let _ = \"line one
line two with } brace\";
}
pub fn g() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn f", "fn g"]);
    }

    #[test]
    fn pub_use_is_not_chunked() {
        let src = "\
pub use crate::foo::Bar;
pub fn f() {
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["fn f"]);
    }

    #[test]
    fn duplicate_method_labels_disambiguated() {
        let src = "\
impl Server {
    pub fn run(&self) {
    }
}

impl Worker {
    pub fn run(&self) {
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["Server::run", "Worker::run"]);
    }

    #[test]
    fn same_type_duplicate_methods_get_suffix() {
        let src = "\
impl Server {
    pub fn run(&self) {
    }
}

impl Server {
    pub fn run(&self, x: u8) {
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["Server::run", "Server::run#2"]);
    }

    #[test]
    fn multi_line_impl_header() {
        let src = "\
impl<T>
    Container<T>
    for Stack<T>
{
    fn len(&self) -> usize {
        self.items.len()
    }
}
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), ["Stack::len"]);
    }

    #[test]
    fn unterminated_fn_errors() {
        let src = "\
pub fn never_closed() {
    let x = 1;
";
        let err = RustParser.parse(src).expect_err("should fail");
        match err {
            ParseError::Unterminated { line } => assert_eq!(line, 1),
            other => panic!("expected Unterminated, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_block_comment_errors() {
        let src = "\
/* never closed
pub fn f() {}
";
        let err = RustParser.parse(src).expect_err("should fail");
        assert!(matches!(err, ParseError::UnterminatedBlockComment { .. }));
    }

    #[test]
    fn registry_recognizes_rs_extension() {
        let parser = super::super::parser_for("rs").expect("registered");
        assert_eq!(parser.language(), Language::Rust);
    }

    #[test]
    fn registry_is_extension_case_insensitive() {
        assert!(super::super::parser_for("RS").is_some());
    }
}
