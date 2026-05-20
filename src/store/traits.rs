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
