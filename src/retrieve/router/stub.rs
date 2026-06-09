use crate::retrieve::router::{Router, RouterError};
use crate::retrieve::{QueryPlan, RouterOutput};

/// Test-only router that returns an empty `QueryPlan` for any input. NOT a
/// production fallback — `auto` mode picks Gemma or Haiku, never this. The
/// whole module is `#[cfg(test)]`-gated to keep that boundary compiler-enforced.
pub(crate) struct StubRouter;

impl Router for StubRouter {
    fn plan(&self, _prompt: &str) -> Result<RouterOutput, RouterError> {
        Ok(RouterOutput::Plan(QueryPlan {
            projects: vec![],
            type_names: vec![],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        }))
    }
}
