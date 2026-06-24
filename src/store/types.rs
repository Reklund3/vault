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

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RetrievalLogEntry {
    pub prompt_hash: String,
    pub query_plan: String,
    pub chunks_returned: u32,
    pub tokens_injected: u32,
}
