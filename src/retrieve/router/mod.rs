use std::str::FromStr;

use crate::config::Config;
use crate::retrieve::{QueryPlan, RouterOutput};
use crate::types::{DocType, Language};
use crate::util::json::extract_json_object;
use crate::util::probe::mlx_reachable;

mod gemma;
mod haiku;
#[cfg(test)]
mod stub;

pub(crate) use gemma::GemmaRouter;
pub(crate) use haiku::HaikuRouter;
#[cfg(test)]
pub(crate) use stub::StubRouter;

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("unparseable query plan: {0}")]
    Unparseable(String),
    #[error("ANTHROPIC_API_KEY not set (required for the haiku router)")]
    MissingApiKey,
}

pub trait Router {
    fn plan(&self, prompt: &str) -> Result<RouterOutput, RouterError>;
}

/// System prompt shared by the Gemma and Haiku routers. It MUST stay
/// byte-identical between the two: the Haiku impl puts it behind
/// `cache_control: ephemeral`, and the Anthropic prompt cache only hits when the
/// cached block matches exactly — divergence silently doubles per-call cost.
pub(crate) const ROUTER_SYSTEM: &str = r#"System prompt:
  "You are a context router for a personal knowledge vault used across software
   engineering, finance, and general project work.
   Extract retrieval signals from the following prompt.
   Respond with JSON only, no other text.

   Schema:
   {
     projects:   [],   // project or service names mentioned or implied
     type_names: [],   // specific named types: proto messages, Go types, API schemas,
                       // account categories, report names, or any named entity
     topics:     [],   // conceptual topics: auth, events, tax, invoicing, grpc, helm, etc
     doc_types:  [],   // which to search: contract, plan, convention, meta
     languages:  []    // go, rust, proto, openapi, markdown, etc
   }

   If nothing warrants retrieval, return { skip: true }."#;

/// Render the user-turn payload for one prompt. The system prompt already
/// specifies the schema; the user turn is just the prompt verbatim.
pub(crate) fn build_user_prompt(prompt: &str) -> String {
    prompt.to_string()
}

#[derive(serde::Deserialize)]
struct RawSkip {
    #[serde(default)]
    skip: bool,
}

#[derive(serde::Deserialize)]
struct RawQueryPlan {
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    type_names: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    doc_types: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
}

impl QueryPlan {
    /// Build a `QueryPlan` from the model's raw string arrays. `doc_types` is
    /// strict — the four-way set is closed, so an unrecognized value is a real
    /// model failure. `languages` is lenient — unknown labels collapse to
    /// `Language::Unknown` so the store still gets a useful filter. Both are
    /// lowercased and trimmed first to tolerate capitalization drift.
    fn from_raw(raw: RawQueryPlan) -> Result<Self, RouterError> {
        let doc_types: Result<Vec<DocType>, _> = raw
            .doc_types
            .into_iter()
            .map(|s| {
                DocType::from_str(&s.trim().to_ascii_lowercase()).map_err(RouterError::Unparseable)
            })
            .collect();
        let languages: Vec<Language> = raw
            .languages
            .into_iter()
            .map(|s| Language::from_str(&s.trim().to_ascii_lowercase()).unwrap_or(Language::Unknown))
            .collect();
        Ok(Self {
            projects: raw.projects,
            type_names: raw.type_names,
            topics: raw.topics,
            doc_types: doc_types?,
            languages,
        })
    }
}

/// Parse a model's free-text reply into a `RouterOutput`. The `{ "skip": true }`
/// shortcut is recognized before the full plan shape, so the hook can bypass
/// the SQLite query entirely for prompts that need no context.
pub(crate) fn parse_response(text: &str) -> Result<RouterOutput, RouterError> {
    let json = extract_json_object(text)
        .ok_or_else(|| RouterError::BadResponse(format!("no JSON object in reply: {text:?}")))?;
    if let Ok(RawSkip { skip: true }) = serde_json::from_str::<RawSkip>(json) {
        return Ok(RouterOutput::Skip);
    }
    let raw: RawQueryPlan = serde_json::from_str(json)
        .map_err(|e| RouterError::BadResponse(format!("invalid JSON: {e}")))?;
    Ok(RouterOutput::Plan(QueryPlan::from_raw(raw)?))
}

/// Which backend `resolve_backend` selected. Kept separate from construction so
/// the hook can interpose UX (none today, but the seam matches the classifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBackend {
    Gemma,
    Haiku,
}

/// Resolve the router backend from `[router].mode`:
/// - `gemma` / `haiku` force that backend.
/// - `auto` (default, and any unrecognized value) probes the local mlx server;
///   reachable → Gemma, otherwise → Haiku.
pub fn resolve_backend(config: &Config) -> ResolvedBackend {
    resolve(config.router_mode(), config.mlx_endpoint())
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

    fn raw(doc_types: &[&str], languages: &[&str]) -> RawQueryPlan {
        RawQueryPlan {
            projects: vec![],
            type_names: vec![],
            topics: vec![],
            doc_types: doc_types.iter().map(|s| s.to_string()).collect(),
            languages: languages.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn from_raw_parses_known_labels() {
        let plan = QueryPlan::from_raw(raw(&["contract", "plan"], &["proto", "rust"])).unwrap();
        assert_eq!(plan.doc_types, vec![DocType::Contract, DocType::Plan]);
        assert_eq!(plan.languages, vec![Language::Proto, Language::Rust]);
    }

    #[test]
    fn from_raw_is_case_insensitive() {
        let plan = QueryPlan::from_raw(raw(&["Contract"], &["Rust"])).unwrap();
        assert_eq!(plan.doc_types, vec![DocType::Contract]);
        assert_eq!(plan.languages, vec![Language::Rust]);
    }

    #[test]
    fn from_raw_unknown_language_is_lenient() {
        let plan = QueryPlan::from_raw(raw(&["convention"], &["python"])).unwrap();
        assert_eq!(plan.languages, vec![Language::Unknown]);
    }

    #[test]
    fn from_raw_unknown_doc_type_is_strict() {
        let err = QueryPlan::from_raw(raw(&["widget"], &[])).unwrap_err();
        assert!(matches!(err, RouterError::Unparseable(_)));
    }

    #[test]
    fn from_raw_empty_arrays_are_fine() {
        let plan = QueryPlan::from_raw(raw(&[], &[])).unwrap();
        assert!(plan.doc_types.is_empty());
        assert!(plan.languages.is_empty());
    }

    #[test]
    fn parse_response_skip_shortcut() {
        let out = parse_response(r#"{ "skip": true }"#).unwrap();
        assert!(matches!(out, RouterOutput::Skip));
    }

    #[test]
    fn parse_response_skip_false_falls_through_to_plan() {
        // Defensive: `{ "skip": false }` has no other fields; the plan shape
        // accepts it with defaults (empty arrays), yielding an empty Plan.
        let out = parse_response(r#"{ "skip": false }"#).unwrap();
        match out {
            RouterOutput::Plan(plan) => {
                assert!(plan.projects.is_empty());
                assert!(plan.doc_types.is_empty());
            }
            RouterOutput::Skip => panic!("expected Plan, got Skip"),
        }
    }

    #[test]
    fn parse_response_full_plan() {
        let text = r#"{
            "projects": ["vault"],
            "type_names": ["BuildRequest"],
            "topics": ["proto"],
            "doc_types": ["contract"],
            "languages": ["proto"]
        }"#;
        let out = parse_response(text).unwrap();
        match out {
            RouterOutput::Plan(plan) => {
                assert_eq!(plan.projects, vec!["vault"]);
                assert_eq!(plan.type_names, vec!["BuildRequest"]);
                assert_eq!(plan.topics, vec!["proto"]);
                assert_eq!(plan.doc_types, vec![DocType::Contract]);
                assert_eq!(plan.languages, vec![Language::Proto]);
            }
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn parse_response_markdown_fenced() {
        let text = "```json\n{\"doc_types\":[\"plan\"],\"languages\":[\"markdown\"]}\n```";
        let out = parse_response(text).unwrap();
        match out {
            RouterOutput::Plan(plan) => assert_eq!(plan.doc_types, vec![DocType::Plan]),
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn parse_response_leading_prose() {
        let text = "Here it is: {\"doc_types\":[\"convention\"],\"languages\":[\"go\"]}";
        let out = parse_response(text).unwrap();
        match out {
            RouterOutput::Plan(plan) => {
                assert_eq!(plan.doc_types, vec![DocType::Convention]);
                assert_eq!(plan.languages, vec![Language::Go]);
            }
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn parse_response_ignores_nested_extra_fields() {
        let text = r#"{"doc_types":["convention"],"languages":["rust"],"meta":{"confidence":0.9}}"#;
        let out = parse_response(text).unwrap();
        match out {
            RouterOutput::Plan(plan) => assert_eq!(plan.doc_types, vec![DocType::Convention]),
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn parse_response_no_json_is_bad_response() {
        let err = parse_response("I don't know.").unwrap_err();
        assert!(matches!(err, RouterError::BadResponse(_)));
    }

    #[test]
    fn parse_response_unknown_doc_type_is_unparseable() {
        let err = parse_response(r#"{"doc_types":["widget"],"languages":["go"]}"#).unwrap_err();
        assert!(matches!(err, RouterError::Unparseable(_)));
    }

    #[test]
    fn parse_response_unknown_language_is_lenient() {
        let text = r#"{"doc_types":["convention"],"languages":["kotlin"]}"#;
        let out = parse_response(text).unwrap();
        match out {
            RouterOutput::Plan(plan) => assert_eq!(plan.languages, vec![Language::Unknown]),
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn resolve_forces_explicit_modes_without_probing() {
        assert_eq!(resolve("gemma", "http://127.0.0.1:1"), ResolvedBackend::Gemma);
        assert_eq!(resolve("haiku", "http://localhost:8080"), ResolvedBackend::Haiku);
    }

    #[test]
    fn resolve_auto_falls_back_to_haiku_when_unreachable() {
        assert_eq!(resolve("auto", "http://127.0.0.1:1"), ResolvedBackend::Haiku);
        assert_eq!(resolve("nonsense", "http://127.0.0.1:1"), ResolvedBackend::Haiku);
    }

    #[test]
    fn build_user_prompt_is_pass_through() {
        assert_eq!(build_user_prompt("what does BuildRequest need?"), "what does BuildRequest need?");
    }

    #[test]
    fn stub_router_returns_fixed_plan() {
        let out = StubRouter.plan("anything").unwrap();
        match out {
            RouterOutput::Plan(plan) => assert!(plan.projects.is_empty()),
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }
}
