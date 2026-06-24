use crate::index::classify::{Classification, Classifier, ClassifyError, ClassifyInput};
use crate::types::{DocType, Language};

/// Test-only classifier that maps a file extension to a plausible
/// `(doc_type, language)`. It exists so the parser → classify → store path can
/// run deterministically in tests without a live Gemma or Anthropic backend.
///
/// This is NOT a production fallback. When both Gemma and Haiku are unreachable,
/// `vault index sync` must prompt the user (or honor an explicit `--type` /
/// `--language` flag) — never silently guess from the extension. The whole
/// module is `#[cfg(test)]`-gated to keep that boundary enforced by the compiler.
pub(crate) struct StubClassifier;

impl Classifier for StubClassifier {
    fn classify(&self, input: &ClassifyInput) -> Result<Classification, ClassifyError> {
        let (doc_type, language) = match input.extension.to_ascii_lowercase().as_str() {
            "proto" => (DocType::Contract, Language::Proto),
            "go" => (DocType::Convention, Language::Go),
            "rs" => (DocType::Convention, Language::Rust),
            "md" => (DocType::Convention, Language::Markdown),
            _ => (DocType::Convention, Language::Unknown),
        };
        Ok(Classification { doc_type, language })
    }
}
