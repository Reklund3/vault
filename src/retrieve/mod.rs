pub mod budget;
pub mod hybrid;
mod router;

pub use router::{ResolvedBackend, Router, build_router, resolve_backend};
// Only the hook's test-only Router stub needs to name RouterError directly.
// Production code only sees it inside the `Result<..., _>` from `Router::plan`,
// which the hook chains through `.ok()?` without referring to the variant.
#[cfg(test)]
pub(crate) use router::{RouterError, StubRouter};

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
