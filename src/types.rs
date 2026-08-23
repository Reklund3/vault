use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocType {
    Contract,
    Plan,
    Convention,
    Meta,
}

impl DocType {
    pub fn as_str(self) -> &'static str {
        match self {
            DocType::Contract => "contract",
            DocType::Plan => "plan",
            DocType::Convention => "convention",
            DocType::Meta => "meta",
        }
    }
}

impl FromStr for DocType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "contract" => Ok(DocType::Contract),
            "plan" => Ok(DocType::Plan),
            "convention" => Ok(DocType::Convention),
            "meta" => Ok(DocType::Meta),
            other => Err(format!(
                "unknown doc_type '{other}' (expected: contract|plan|convention|meta)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Go,
    Rust,
    Scala,
    Proto,
    OpenApi,
    Helm,
    Markdown,
    Unknown,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Scala => "scala",
            Language::Proto => "proto",
            Language::OpenApi => "openapi",
            Language::Helm => "helm",
            Language::Markdown => "markdown",
            Language::Unknown => "unknown",
        }
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "go" => Ok(Language::Go),
            "rust" => Ok(Language::Rust),
            "scala" => Ok(Language::Scala),
            "proto" => Ok(Language::Proto),
            "openapi" => Ok(Language::OpenApi),
            "helm" => Ok(Language::Helm),
            "markdown" => Ok(Language::Markdown),
            "unknown" => Ok(Language::Unknown),
            other => Err(format!(
                "unknown language '{other}' (expected: go|rust|scala|proto|openapi|helm|markdown|unknown)"
            )),
        }
    }
}

/// A snapshot of what is actually indexed, taken from the store and handed to
/// the router so it stops guessing.
///
/// The router is otherwise blind to the corpus: `QueryPlanner` holds no
/// connection by design (see `crate::vault`), and the user turn was the prompt
/// verbatim, so the model picked `languages` off the example list in the system
/// prompt. Against a Rust-only vault that reliably produced `languages:
/// ["go"]` — enum-valid, so `QueryPlan::from_raw`'s drop-unrecognized guard let
/// it through, and it matched zero chunks.
///
/// This is a **snapshot**, not a live view. It is read once when the planner is
/// built and can go stale against a concurrent `index sync`; that is deliberate,
/// since re-reading it per call would put a SQLite query back on the
/// `Send + Sync` half of the pipeline. Stale only costs plan quality — every
/// value is still filtered by the store at query time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub projects: Vec<String>,
    pub languages: Vec<Language>,
    pub doc_types: Vec<DocType>,
}

impl Inventory {
    /// No inventory available — an unindexed store, a backend that does not
    /// implement the query, or a planner built without one.
    ///
    /// This means **"unknown"**, never "nothing is indexed". Both consumers
    /// treat it as a no-op: no grounding is added to the router prompt, and
    /// `QueryPlan::retain_indexed` prunes nothing. Reading it the other way
    /// would make an empty inventory strip every filter off every plan.
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty() && self.languages.is_empty() && self.doc_types.is_empty()
    }
}
