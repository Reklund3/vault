pub mod budget;
pub mod hybrid;
mod router;

pub use router::{ResolvedBackend, Router, RouterError, build_router, resolve_backend};
// `RouterError` is named by `VaultError::{RouterBuild, RouterPlan}`, so it is
// part of the public surface now — it used to be `#[cfg(test)]` back when
// production code only ever saw it through `Display`. The stub stays test-only:
// that gating is what stops a stub becoming a silent production fallback.
#[cfg(test)]
pub(crate) use router::StubRouter;

use crate::types::{DocType, Language};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum RouterOutput {
    Skip,
    Plan(QueryPlan),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    pub projects: Vec<String>,
    pub type_names: Vec<String>,
    pub topics: Vec<String>,
    pub doc_types: Vec<DocType>,
    pub languages: Vec<Language>,
}
