mod stub;
mod tei;

pub use stub::StubEmbedder;
pub use tei::TeiEmbedder;

pub trait Embedder {
    /// Embedding dimension this embedder produces. Used by callers (the store,
    /// validation paths) to confirm three-way agreement: config ⟷ schema ⟷ server.
    fn dim(&self) -> usize;

    fn embed_document(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
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
