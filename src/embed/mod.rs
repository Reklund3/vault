mod stub;
mod tei;

pub use stub::StubEmbedder;
pub use tei::TeiEmbedder;

pub trait Embedder {
    /// Embedding dimension this embedder produces. Used by callers (the store,
    /// validation paths) to confirm three-way agreement: config ⟷ schema ⟷ server.
    #[allow(dead_code)]
    fn dim(&self) -> usize;

    fn embed_document(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError>;

    /// Embed many documents in one shot. The default loops over `embed_document`
    /// (correct for any backend); `TeiEmbedder` overrides it to issue a single
    /// batched HTTP request per server batch. Returns exactly one vector per
    /// input, in input order — callers zip the result against their chunks.
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts.iter().map(|t| self.embed_document(t)).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("dim mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
    #[error("model mismatch: config expects {expected}, server reports {actual}")]
    ModelMismatch { expected: String, actual: String },
}
