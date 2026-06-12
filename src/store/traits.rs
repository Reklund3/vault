use crate::retrieve::QueryPlan;
use crate::store::types::{ChunkWithEmbedding, Document, Hit, RetrievalLogEntry};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("not found")]
    #[allow(dead_code)]
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
    fn get_or_create_project(&mut self, name: &str, repo_path: &str) -> Result<i64, StoreError>;

    fn upsert_document(
        &mut self,
        doc: &Document,
        chunks: &[ChunkWithEmbedding],
    ) -> Result<(), StoreError>;

    /// Look up the stored `content_hash` for a document by `(project_id, source_path)`.
    /// Returns `Ok(None)` when no such document exists. Used by sync's
    /// unchanged-file gate to skip classify + parse + embed when the on-disk
    /// content hasn't drifted since the last sync.
    fn get_document_content_hash(
        &self,
        project_id: i64,
        source_path: &str,
    ) -> Result<Option<String>, StoreError>;

    fn prune_orphans(
        &mut self,
        project_id: i64,
        kept_paths: &[String],
    ) -> Result<usize, StoreError>;

    /// Raw BM25 keyword search. Returns up to `top_k` hits with `bm25_score`
    /// populated (other score fields left 0); an empty result is valid (e.g. the
    /// plan carried no keyword tokens). Filtering by `plan.projects` /
    /// `doc_types` / `languages` is applied here.
    fn bm25_search(&self, plan: &QueryPlan, top_k: usize) -> Result<Vec<Hit>, StoreError>;

    /// Raw cosine-similarity search over stored embeddings. Returns up to `top_k`
    /// hits with `cosine_score` populated. Validates `embedding` length against
    /// the backend's configured dim and applies the same plan filters as
    /// `bm25_search`.
    fn cosine_search(
        &self,
        plan: &QueryPlan,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<Hit>, StoreError>;

    /// Hybrid retrieval: run both primitive queries and blend them with the
    /// shared ranking math in [`crate::retrieve::hybrid`].
    ///
    /// **Provided, not implemented per-backend.** Real backends (SQLite,
    /// Postgres, …) implement only `bm25_search` and `cosine_search`, so the
    /// scoring is byte-identical across all of them and tunes in one place. Test
    /// doubles may override this to return canned hits without running the merge.
    fn hybrid_search(
        &self,
        plan: &QueryPlan,
        embedding: &[f32],
        alpha: f32,
    ) -> Result<Vec<Hit>, StoreError> {
        use crate::retrieve::hybrid::{TOP_K, merge};
        let bm25 = self.bm25_search(plan, TOP_K)?;
        let cosine = self.cosine_search(plan, embedding, TOP_K)?;
        Ok(merge(bm25, cosine, alpha, TOP_K))
    }

    #[allow(dead_code)]
    fn log_retrieval(&mut self, entry: &RetrievalLogEntry) -> Result<(), StoreError>;
}
