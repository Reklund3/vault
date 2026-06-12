pub mod budget;
pub mod hybrid;
mod router;

pub use router::{ResolvedBackend, Router, build_router, resolve_backend};
// Only test-only Router stubs need to name RouterError directly. Production
// code sees it inside the `Result<..., _>` from `Router::plan` and records it
// via `Display` into hook.log without ever naming a variant.
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
