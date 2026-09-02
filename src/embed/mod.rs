mod stub;
mod tei;

// `StubEmbedder` exists for `vault diagnose --stub`, which traces retrieval
// plumbing without TEI. It must NOT be `#[cfg(test)]`-gated — that is the
// deliberate asymmetry with the router and classifier stubs (see CLAUDE.md), and
// it still holds: this ships in every release build that has a CLI, which is
// exactly when `--stub` exists. `test` is in the gate because the facade and
// hook unit tests use it as a cheap deterministic embedder.
#[cfg(any(feature = "cli", test))]
pub use stub::StubEmbedder;
pub use tei::TeiEmbedder;

pub trait Embedder {
    /// Embedding dimension this embedder produces.
    ///
    /// Part of the trait contract, exercised only by test doubles today — the
    /// config ⟷ schema ⟷ server agreement it was meant to check is enforced
    /// instead by `TeiEmbedder::verify_against_server` and the store's
    /// `(model, dim)` lock. Kept because an `Embedder` that cannot state its
    /// dimension cannot be validated at all; the previous comment claimed
    /// callers in the store and validation paths, and there were none.
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
