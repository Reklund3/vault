use crate::retrieve::QueryPlan;
use crate::store::types::{ChunkWithEmbedding, Document, Hit, RetrievalLogEntry};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("not found")]
    NotFound,
    #[error("integrity violation: {0}")]
    Conflict(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error(
        "incompatible embedding: DB has ({stored_model}, dim {stored_dim}); \
         config requires ({expected_model}, dim {expected_dim}). \
         Run `vault embed migrate` to re-embed against the new model."
    )]
    IncompatibleEmbedding {
        stored_model: String,
        stored_dim: usize,
        expected_model: String,
        expected_dim: usize,
    },
}

pub trait Store {
    fn migrate(&mut self) -> Result<(), StoreError>;

    /// Insert-or-fetch a project row by name. Returns the project id.
    ///
    /// Behavior:
    /// - Name absent → insert `(name, repo_path)` and return the new id.
    /// - Name present with matching `repo_path` (or NULL on the existing row) →
    ///   return the existing id. NULL on the existing row is treated as
    ///   "matches anything" so legacy/test rows without a path keep working.
    /// - Name present with a different non-null `repo_path` → `StoreError::Conflict`.
    ///   The caller (the indexer) maps this to a user-facing "pass --name" hint.
    fn get_or_create_project(
        &mut self,
        name: &str,
        repo_path: &str,
    ) -> Result<i64, StoreError>;

    fn upsert_document(
        &mut self,
        doc: &Document,
        chunks: &[ChunkWithEmbedding],
    ) -> Result<(), StoreError>;

    fn prune_orphans(
        &mut self,
        project_id: i64,
        kept_paths: &[String],
    ) -> Result<usize, StoreError>;

    fn hybrid_search(
        &self,
        plan: &QueryPlan,
        embedding: &[f32],
        alpha: f32,
    ) -> Result<Vec<Hit>, StoreError>;

    fn log_retrieval(&mut self, entry: &RetrievalLogEntry) -> Result<(), StoreError>;
}
