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

    /// Resolve the domain assigned to the first of `project_names` (in router
    /// order) that has one. Case-insensitive name match. Returns `Ok(None)` when
    /// no named project is assigned a domain — the hook then derives the tag
    /// from `defaults.context_tag` instead of `{domain}-context`.
    ///
    /// **Provided default returns `Ok(None)`** (every project unassigned).
    /// Backends that persist `projects.domain` override this; test doubles and
    /// the Postgres placeholder inherit the no-domain default.
    fn resolve_domain(&self, _project_names: &[String]) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    /// Assign `domain` to a project row, overwriting any existing assignment.
    /// Used by the first-run domain prompt in `vault index sync`.
    ///
    /// **Provided default is a no-op** (`Ok(())`) so test doubles and the
    /// Postgres placeholder need not implement it; backends that persist
    /// `projects.domain` override it.
    fn set_project_domain(&mut self, _project_id: i64, _domain: &str) -> Result<(), StoreError> {
        Ok(())
    }

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
        let hits = merge(bm25, cosine, alpha, TOP_K);

        // Filter-trap fallback for `languages`/`doc_types`. These are enum-valid
        // by the time they reach here (QueryPlan::from_raw drops unrecognized
        // values), so — unlike a phantom `projects` name, which is degraded at
        // value-resolution time — the only way they go wrong is a *valid* value
        // that matches zero chunks (e.g. languages=["proto"] against a Rust-only
        // repo, where the answer lives in rust-classified chunks). That failure
        // is result-level, not value-level: it's invisible until the filtered
        // query returns nothing, and per-field existence checks would miss the
        // case where each field has chunks but their AND-combination is empty.
        // So we key off the actual result. If the filtered pass found nothing
        // and we had one of these structural filters to relax, retry once with
        // both dropped, reusing the same embedding — the retry is SQL-only, no
        // re-embed (matters on the hook hot path). `projects` is left intact:
        // it's already degraded by `existing_project_ids`, so this composes with
        // that fix rather than duplicating it. Downstream `min_score` gating
        // still applies to the relaxed hits, so a genuinely-empty corpus returns
        // nothing rather than unbounded noise.
        if hits.is_empty() && (!plan.languages.is_empty() || !plan.doc_types.is_empty()) {
            let mut relaxed = plan.clone();
            relaxed.languages.clear();
            relaxed.doc_types.clear();
            let bm25 = self.bm25_search(&relaxed, TOP_K)?;
            let cosine = self.cosine_search(&relaxed, embedding, TOP_K)?;
            return Ok(merge(bm25, cosine, alpha, TOP_K));
        }

        Ok(hits)
    }

    #[allow(dead_code)]
    fn log_retrieval(&mut self, entry: &RetrievalLogEntry) -> Result<(), StoreError>;
}
