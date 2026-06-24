/// Return the first balanced `{...}` substring, honoring quoted strings so a `}`
/// inside a string value doesn't close the object early. Used to extract the
/// JSON payload from model replies that may include markdown fences or
/// surrounding prose.
pub(crate) fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0u32;
    let mut in_string = false;
    let mut escape = false;
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_object() {
        assert_eq!(extract_json_object(r#"{"a":1}"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extracts_object_inside_fences() {
        let text = "```json\n{\"a\":1}\n```";
        assert_eq!(extract_json_object(text), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extracts_object_with_leading_prose() {
        let text = "Sure: {\"a\":1}";
        assert_eq!(extract_json_object(text), Some(r#"{"a":1}"#));
    }

    #[test]
    fn handles_nested_objects() {
        let text = r#"{"a":1,"b":{"c":2}}"#;
        assert_eq!(extract_json_object(text), Some(text));
    }

    #[test]
    fn ignores_braces_inside_strings() {
        let text = r#"{"a":"}"}"#;
        assert_eq!(extract_json_object(text), Some(text));
    }

    #[test]
    fn handles_escaped_quotes() {
        let text = r#"{"a":"\"}"}"#;
        assert_eq!(extract_json_object(text), Some(text));
    }

    #[test]
    fn returns_none_when_no_object() {
        assert_eq!(extract_json_object("no json here"), None);
    }
}
