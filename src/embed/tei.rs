use std::time::Duration;

use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::embed::{EmbedError, Embedder};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct TeiEmbedder {
    endpoint: Url,
    expected_model: String,
    dim: usize,
    http: Client,
}

impl TeiEmbedder {
    pub fn from_config(config: &Config) -> Result<Self, EmbedError> {
        Self::from_config_with_timeout(config, DEFAULT_TIMEOUT)
    }

    pub fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, EmbedError> {
        let endpoint = Url::parse(config.embedding_endpoint())
            .map_err(|e| EmbedError::Transport(format!("bad endpoint: {e}")))?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| EmbedError::Transport(e.to_string()))?;
        Ok(Self {
            endpoint,
            expected_model: config.embedding_model().to_string(),
            dim: config.embedding_dim(),
            http,
        })
    }

    /// Probe the server with one test embedding. Closes the third leg of the
    /// config ⟷ schema ⟷ server agreement check: verifies the server returns
    /// vectors at the configured dim, and — if the server reports its model —
    /// that the model name matches what the config declares.
    ///
    /// Call once at startup. Failure means there is no graceful degradation:
    /// indexing into a schema locked at one dim with a server producing
    /// another silently corrupts retrieval. Refuse to proceed.
    pub fn verify_against_server(&self) -> Result<(), EmbedError> {
        let (model, vec) = self.embed_with_model("search_query: ping")?;

        if vec.len() != self.dim {
            return Err(EmbedError::DimensionMismatch {
                expected: self.dim,
                actual: vec.len(),
            });
        }

        if let Some(server_model) = model
            && server_model != self.expected_model
        {
            return Err(EmbedError::ModelMismatch {
                expected: self.expected_model.clone(),
                actual: server_model,
            });
        }

        Ok(())
    }

    fn embed(&self, prefixed: &str) -> Result<Vec<f32>, EmbedError> {
        let (_, vec) = self.embed_with_model(prefixed)?;
        if vec.len() != self.dim {
            return Err(EmbedError::DimensionMismatch {
                expected: self.dim,
                actual: vec.len(),
            });
        }
        Ok(vec)
    }

    fn embed_with_model(&self, prefixed: &str) -> Result<(Option<String>, Vec<f32>), EmbedError> {
        let url = self
            .endpoint
            .join("/v1/embeddings")
            .map_err(|e| EmbedError::Transport(e.to_string()))?;

        let resp = self
            .http
            .post(url)
            .json(&EmbedRequest { input: prefixed })
            .send()
            .map_err(|e| EmbedError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(EmbedError::BadResponse(format!("HTTP {status}: {body}")));
        }

        let body: EmbedResponse = resp
            .json()
            .map_err(|e| EmbedError::BadResponse(e.to_string()))?;

        let model = body.model;
        let emb = body
            .data
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::BadResponse("empty data array".to_string()))?
            .embedding;

        Ok((model, emb))
    }
}

impl Embedder for TeiEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_document(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed(&format!("search_document: {text}"))
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed(&format!("search_query: {text}"))
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
    /// TEI populates this with the loaded model id. Optional defensively in
    /// case a future server version omits it.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires live TEI at http://localhost:8081"]
    fn live_tei_probe_and_embed() {
        let config = Config::default();
        let client = TeiEmbedder::from_config(&config).expect("client");

        client.verify_against_server().expect("server probe");

        let doc = client.embed_document("hello world").expect("doc embed");
        assert_eq!(doc.len(), config.embedding_dim());

        let q = client.embed_query("hello world").expect("query embed");
        assert_eq!(q.len(), config.embedding_dim());
    }
}
