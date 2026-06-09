use crate::retrieve::QueryPlan;
use crate::store::traits::{Store, StoreError};
use crate::store::types::{ChunkWithEmbedding, Document, Hit, RetrievalLogEntry};

// Placeholder for the future distributed backend (tsvector + pgvector).
// Methods stay as `todo!()` until vault gains a real distribution requirement.
pub struct PostgresStore {}

impl Store for PostgresStore {
    fn migrate(&mut self) -> Result<(), StoreError> {
        todo!()
    }

    fn get_or_create_project(&mut self, _name: &str, _repo_path: &str) -> Result<i64, StoreError> {
        todo!()
    }

    fn get_document_content_hash(
        &self,
        _project_id: i64,
        _source_path: &str,
    ) -> Result<Option<String>, StoreError> {
        todo!()
    }

    fn upsert_document(
        &mut self,
        _doc: &Document,
        _chunks: &[ChunkWithEmbedding],
    ) -> Result<(), StoreError> {
        todo!()
    }

    fn prune_orphans(
        &mut self,
        _project_id: i64,
        _kept_paths: &[String],
    ) -> Result<usize, StoreError> {
        todo!()
    }

    // Implement only the two primitives; `hybrid_search` is the trait's provided
    // method, so this backend inherits the exact same ranking math as SqliteStore
    // (tsvector ts_rank for BM25, pgvector `<=>` cosine, mapped into Hit).
    fn bm25_search(&self, _plan: &QueryPlan, _top_k: usize) -> Result<Vec<Hit>, StoreError> {
        todo!()
    }

    fn cosine_search(
        &self,
        _plan: &QueryPlan,
        _embedding: &[f32],
        _top_k: usize,
    ) -> Result<Vec<Hit>, StoreError> {
        todo!()
    }

    fn log_retrieval(&mut self, _entry: &RetrievalLogEntry) -> Result<(), StoreError> {
        todo!()
    }
}
