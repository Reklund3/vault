use std::str::FromStr;

use crate::config::Config;
use crate::types::{DocType, Language};
use crate::util::json::extract_json_object;
use crate::util::probe::mlx_reachable;

mod gemma;
mod haiku;
#[cfg(test)]
mod stub;

pub(crate) use gemma::GemmaClassifier;
pub(crate) use haiku::{HaikuClassifier, cost_estimate};
#[cfg(test)]
pub(crate) use stub::StubClassifier;

/// What the classifier sees about a file. Bounded to filename, extension, and
/// the first ~1KB of content — full files never reach the classifier (and so
/// never reach Anthropic in Haiku mode); they reach Anthropic only via
/// retrieval-time injection, which the user controls.
pub struct ClassifyInput {
    pub filename: String,
    pub extension: String,
    pub head: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub doc_type: DocType,
    pub language: Language,
}

impl Classification {
    /// Build a classification from the model's raw string labels. `doc_type` is
    /// strict — the four-way set is closed, so an unrecognized value is a real
    /// model failure. `language` is lenient — an unknown value maps to
    /// `Language::Unknown`, which is exactly the case `vault index sync` turns
    /// into an interactive "confirm or override" prompt. Both are lowercased
    /// first to tolerate capitalization drift from the model.
    pub(crate) fn from_strings(doc_type: &str, language: &str) -> Result<Self, ClassifyError> {
        let doc_type = DocType::from_str(&doc_type.trim().to_ascii_lowercase())
            .map_err(ClassifyError::Unparseable)?;
        let language =
            Language::from_str(&language.trim().to_ascii_lowercase()).unwrap_or(Language::Unknown);
        Ok(Self { doc_type, language })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("unparseable classification: {0}")]
    Unparseable(String),
    #[error("ANTHROPIC_API_KEY not set (required for the haiku classifier)")]
    MissingApiKey,
}

pub trait Classifier {
    fn classify(&self, input: &ClassifyInput) -> Result<Classification, ClassifyError>;
}

/// System prompt shared by the Gemma and Haiku classifiers. It MUST stay
/// byte-identical between the two: the Haiku impl puts it behind
/// `cache_control: ephemeral`, and the Anthropic prompt cache only hits when the
/// cached block matches exactly — divergence silently doubles per-call cost.
pub(crate) const CLASSIFY_SYSTEM: &str = r#"You classify a source file for a code-context index used by software engineers.

You are given a file's name, extension, and the first 1KB of its content. Respond with JSON only — no prose, no markdown fences:

{"doc_type": "<contract|plan|convention|meta>", "language": "<go|rust|scala|proto|openapi|helm|markdown|unknown>"}

doc_type:
- contract:   API/interface definitions — protobuf, OpenAPI/Swagger specs
- plan:       design docs, RFCs, proposals (prose describing intended work)
- convention: source code and coding-convention docs — Go/Rust/Scala source, CLAUDE.md-style guidance
- meta:       repository meta docs — READMEs, contributing guides, changelogs

language: the file's source language, or "unknown" if it cannot be determined.

Rules:
- Always return exactly one doc_type from {contract, plan, convention, meta}.
- If you cannot determine the language, return "unknown".

Examples:

Input: file "build.proto" (proto)
---
syntax = "proto3";
message BuildRequest { string id = 1; }
Output: {"doc_type": "contract", "language": "proto"}

Input: file "CONVENTIONS.md" (md)
---
# Error handling
Always wrap errors with context.
Output: {"doc_type": "convention", "language": "markdown"}"#;

/// Render the user-turn prompt for one file, matching the few-shot framing in
/// `CLASSIFY_SYSTEM`.
pub(crate) fn build_user_prompt(input: &ClassifyInput) -> String {
    format!(
        "Input: file {:?} ({})\n---\n{}",
        input.filename, input.extension, input.head
    )
}

#[derive(serde::Deserialize)]
struct RawClassification {
    #[serde(default)]
    doc_type: String,
    #[serde(default)]
    language: String,
}

/// Parse a model's free-text reply into a `Classification`. Tolerates markdown
/// fences and surrounding prose by extracting the first balanced `{...}` object;
/// a reply with no JSON object is `BadResponse`, valid JSON with an unknown
/// `doc_type` is `Unparseable` (see `Classification::from_strings`).
pub(crate) fn parse_response(text: &str) -> Result<Classification, ClassifyError> {
    let json = extract_json_object(text)
        .ok_or_else(|| ClassifyError::BadResponse(format!("no JSON object in reply: {text:?}")))?;
    let raw: RawClassification = serde_json::from_str(json)
        .map_err(|e| ClassifyError::BadResponse(format!("invalid JSON: {e}")))?;
    Classification::from_strings(&raw.doc_type, &raw.language)
}

/// Which backend `resolve_backend` selected. Kept separate from construction so
/// `vault index sync` can interpose its one-time cost-confirmation prompt before
/// building a `HaikuClassifier` when auto-mode falls back to remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBackend {
    Gemma,
    Haiku,
}

/// Resolve the classifier backend from `[classifier].mode`:
/// - `gemma` / `haiku` force that backend.
/// - `auto` (default, and any unrecognized value) probes the local mlx server;
///   reachable → Gemma, otherwise → Haiku.
pub fn resolve_backend(config: &Config) -> ResolvedBackend {
    resolve(config.classifier_mode(), config.mlx_endpoint())
}

fn resolve(mode: &str, mlx_endpoint: &str) -> ResolvedBackend {
    match mode {
        "gemma" => ResolvedBackend::Gemma,
        "haiku" => ResolvedBackend::Haiku,
        _ => {
            if mlx_reachable(mlx_endpoint) {
                ResolvedBackend::Gemma
            } else {
                ResolvedBackend::Haiku
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_strings_parses_known_labels() {
        let c = Classification::from_strings("contract", "proto").unwrap();
        assert_eq!(c.doc_type, DocType::Contract);
        assert_eq!(c.language, Language::Proto);
    }

    #[test]
    fn from_strings_is_case_insensitive() {
        let c = Classification::from_strings("Convention", "Rust").unwrap();
        assert_eq!(c.doc_type, DocType::Convention);
        assert_eq!(c.language, Language::Rust);
    }

    #[test]
    fn from_strings_unknown_language_is_lenient() {
        let c = Classification::from_strings("convention", "python").unwrap();
        assert_eq!(c.doc_type, DocType::Convention);
        assert_eq!(c.language, Language::Unknown);
    }

    #[test]
    fn from_strings_unknown_doc_type_is_strict() {
        let err = Classification::from_strings("widget", "go").unwrap_err();
        assert!(matches!(err, ClassifyError::Unparseable(_)));
    }

    #[test]
    fn resolve_forces_explicit_modes_without_probing() {
        assert_eq!(resolve("gemma", "http://127.0.0.1:1"), ResolvedBackend::Gemma);
        assert_eq!(resolve("haiku", "http://localhost:8080"), ResolvedBackend::Haiku);
    }

    #[test]
    fn resolve_auto_falls_back_to_haiku_when_unreachable() {
        // Port 1 is privileged and not served — the probe fails fast.
        assert_eq!(resolve("auto", "http://127.0.0.1:1"), ResolvedBackend::Haiku);
        // Unrecognized modes are treated as auto.
        assert_eq!(resolve("nonsense", "http://127.0.0.1:1"), ResolvedBackend::Haiku);
    }

    #[test]
    fn parse_response_bare_json() {
        let c = parse_response(r#"{"doc_type":"contract","language":"proto"}"#).unwrap();
        assert_eq!(c.doc_type, DocType::Contract);
        assert_eq!(c.language, Language::Proto);
    }

    #[test]
    fn parse_response_markdown_fenced() {
        let text = "```json\n{\"doc_type\": \"plan\", \"language\": \"markdown\"}\n```";
        let c = parse_response(text).unwrap();
        assert_eq!(c.doc_type, DocType::Plan);
        assert_eq!(c.language, Language::Markdown);
    }

    #[test]
    fn parse_response_leading_prose() {
        let text = "Sure, here is the classification:\n{\"doc_type\":\"convention\",\"language\":\"go\"}";
        let c = parse_response(text).unwrap();
        assert_eq!(c.doc_type, DocType::Convention);
        assert_eq!(c.language, Language::Go);
    }

    #[test]
    fn parse_response_trailing_prose() {
        let text = "{\"doc_type\":\"meta\",\"language\":\"markdown\"}\nLet me know if you need more.";
        let c = parse_response(text).unwrap();
        assert_eq!(c.doc_type, DocType::Meta);
    }

    #[test]
    fn parse_response_ignores_nested_extra_object() {
        let text = r#"{"doc_type":"convention","language":"rust","meta":{"confidence":0.9}}"#;
        let c = parse_response(text).unwrap();
        assert_eq!(c.doc_type, DocType::Convention);
        assert_eq!(c.language, Language::Rust);
    }

    #[test]
    fn parse_response_no_json_is_bad_response() {
        let err = parse_response("I don't know.").unwrap_err();
        assert!(matches!(err, ClassifyError::BadResponse(_)));
    }

    #[test]
    fn parse_response_unknown_language_is_lenient() {
        let c = parse_response(r#"{"doc_type":"convention","language":"kotlin"}"#).unwrap();
        assert_eq!(c.language, Language::Unknown);
    }

    #[test]
    fn parse_response_unknown_doc_type_is_unparseable() {
        let err = parse_response(r#"{"doc_type":"widget","language":"go"}"#).unwrap_err();
        assert!(matches!(err, ClassifyError::Unparseable(_)));
    }

    #[test]
    fn build_user_prompt_includes_file_facts() {
        let input = ClassifyInput {
            filename: "build.proto".to_string(),
            extension: "proto".to_string(),
            head: "syntax = \"proto3\";".to_string(),
        };
        let prompt = build_user_prompt(&input);
        assert!(prompt.contains("build.proto"));
        assert!(prompt.contains("(proto)"));
        assert!(prompt.contains("syntax = \"proto3\";"));
    }

    #[test]
    fn stub_classifies_by_extension() {
        let cases = [
            ("proto", DocType::Contract, Language::Proto),
            ("go", DocType::Convention, Language::Go),
            ("rs", DocType::Convention, Language::Rust),
            ("md", DocType::Convention, Language::Markdown),
            ("xyz", DocType::Convention, Language::Unknown),
        ];
        for (ext, doc_type, language) in cases {
            let input = ClassifyInput {
                filename: format!("file.{ext}"),
                extension: ext.to_string(),
                head: String::new(),
            };
            let c = StubClassifier.classify(&input).unwrap();
            assert_eq!(c.doc_type, doc_type, "ext {ext}");
            assert_eq!(c.language, language, "ext {ext}");
        }
    }
}
