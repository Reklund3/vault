use crate::types::{DocType, Language};

#[derive(Debug, Clone)]
pub struct Document {
    pub project_id: i64,
    pub doc_type: DocType,
    pub source_path: String,
    pub title: String,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub language: Language,
    pub label: String,
    pub content: String,
    pub content_hash: String,
    pub token_est: u32,
    pub chunk_index: u32,
}

#[derive(Debug, Clone)]
pub struct ChunkWithEmbedding {
    pub chunk: Chunk,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub chunk_id: i64,
    /// Populated by `map_hit_row` from the same row as everything else. Nothing
    /// downstream branches on it — retrieval filters by project in SQL, not
    /// after the fact — so only the store's own tests read it back, to prove a
    /// project filter actually bound.
    #[allow(dead_code)]
    pub project_id: i64,
    pub doc_type: DocType,
    pub label: String,
    pub content: String,
    pub token_est: u32,
    pub bm25_score: f32,
    pub cosine_score: f32,
    pub final_score: f32,
}

/// A row for the `retrieval_log` table.
///
/// The table and `Store::log_retrieval` both exist and are implemented; nothing
/// on the hook path calls them yet. Retained as the unfinished half of a
/// planned feature, not as an accident — see `retrieval_log` in the runtime-data
/// table in CLAUDE.md.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RetrievalLogEntry {
    pub prompt_hash: String,
    pub query_plan: String,
    pub chunks_returned: u32,
    pub tokens_injected: u32,
}
