use std::time::Duration;

use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::embed::{EmbedError, Embedder};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Max inputs per batched `/v1/embeddings` request. TEI rejects a batch larger
/// than its `--max-client-batch-size` (default 32) with HTTP 413, so
/// `embed_documents` sub-batches to this cap. A single file post-5B can window
/// into many chunks, so this guard is load-bearing, not theoretical.
const MAX_CLIENT_BATCH: usize = 32;

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
        let body = self.post_embeddings(&EmbedRequest { input: prefixed })?;
        let model = body.model;
        let emb = body
            .data
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::BadResponse("empty data array".to_string()))?
            .embedding;
        Ok((model, emb))
    }

    /// Embed one batch (already prefixed, already capped to `MAX_CLIENT_BATCH`).
    /// Trusts the server to return results in input order — the same assumption
    /// the single path makes with `data[0]` — and verifies that with a strict
    /// count + per-vector dim check in `assemble_batch`.
    fn embed_batch(&self, prefixed: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let body = self.post_embeddings(&EmbedBatchRequest { input: prefixed })?;
        assemble_batch(body.data, prefixed.len(), self.dim)
    }

    /// Single POST to `/v1/embeddings`, shared by the single and batch paths so
    /// the transport + status handling can't drift between them. `body` is
    /// either `EmbedRequest` (string) or `EmbedBatchRequest` (array); TEI's
    /// OpenAI-compatible endpoint accepts both shapes of `input`.
    fn post_embeddings<B: Serialize>(&self, body: &B) -> Result<EmbedResponse, EmbedError> {
        let url = self
            .endpoint
            .join("/v1/embeddings")
            .map_err(|e| EmbedError::Transport(e.to_string()))?;

        let resp = self
            .http
            .post(url)
            .json(body)
            .send()
            .map_err(|e| EmbedError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(EmbedError::BadResponse(format!("HTTP {status}: {body}")));
        }

        resp.json()
            .map_err(|e| EmbedError::BadResponse(e.to_string()))
    }
}

/// Pure assembly of a batch response: enforce one vector per input (order
/// trusted, count checked) and the configured dim on each. Split out from the
/// HTTP path so it is exercised in CI — the live `embed_*` tests are `#[ignore]`
/// and never run there.
fn assemble_batch(
    data: Vec<EmbedData>,
    expected_n: usize,
    expected_dim: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    if data.len() != expected_n {
        return Err(EmbedError::BadResponse(format!(
            "batch size mismatch: sent {expected_n}, server returned {}",
            data.len()
        )));
    }
    data.into_iter()
        .map(|d| {
            if d.embedding.len() != expected_dim {
                Err(EmbedError::DimensionMismatch {
                    expected: expected_dim,
                    actual: d.embedding.len(),
                })
            } else {
                Ok(d.embedding)
            }
        })
        .collect()
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

    /// One batched request per `MAX_CLIENT_BATCH` window, concatenated in order.
    /// Empty input makes no HTTP call. Callers (sync) batch *per file*, so a
    /// failure here skips exactly one file — same granularity as the old
    /// per-chunk loop.
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let prefixed: Vec<String> = texts
            .iter()
            .map(|t| format!("search_document: {t}"))
            .collect();
        let mut out = Vec::with_capacity(prefixed.len());
        for window in prefixed.chunks(MAX_CLIENT_BATCH) {
            out.append(&mut self.embed_batch(window)?);
        }
        Ok(out)
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: &'a str,
}

#[derive(Serialize)]
struct EmbedBatchRequest<'a> {
    input: &'a [String],
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

    fn data(vecs: &[&[f32]]) -> Vec<EmbedData> {
        vecs.iter()
            .map(|v| EmbedData {
                embedding: v.to_vec(),
            })
            .collect()
    }

    #[test]
    fn assemble_batch_preserves_order_and_count() {
        let d = data(&[&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0], &[0.0, 0.0, 1.0]]);
        let out = assemble_batch(d, 3, 3).expect("ok");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], vec![1.0, 0.0, 0.0]);
        assert_eq!(out[1], vec![0.0, 1.0, 0.0]);
        assert_eq!(out[2], vec![0.0, 0.0, 1.0]);
    }

    #[test]
    fn assemble_batch_rejects_short_response() {
        // Server returned fewer vectors than inputs — must error, never silently
        // drop a chunk (a zip on the caller side would have hidden this).
        let d = data(&[&[1.0, 0.0], &[0.0, 1.0]]);
        let err = assemble_batch(d, 3, 2).expect_err("count mismatch");
        assert!(matches!(err, EmbedError::BadResponse(_)));
    }

    #[test]
    fn assemble_batch_rejects_wrong_dim() {
        let d = data(&[&[1.0, 0.0, 0.0], &[0.0, 1.0]]); // second is dim 2, not 3
        let err = assemble_batch(d, 2, 3).expect_err("dim mismatch");
        assert!(matches!(
            err,
            EmbedError::DimensionMismatch {
                expected: 3,
                actual: 2
            }
        ));
    }

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

    #[test]
    #[ignore = "requires live TEI at http://localhost:8081"]
    fn live_tei_batch_embed_subbatches() {
        let config = Config::default();
        let client = TeiEmbedder::from_config(&config).expect("client");

        // More than MAX_CLIENT_BATCH so the sub-batch loop runs at least twice.
        let texts: Vec<String> = (0..MAX_CLIENT_BATCH + 5)
            .map(|i| format!("chunk number {i}"))
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();

        let out = client.embed_documents(&refs).expect("batch embed");
        assert_eq!(out.len(), refs.len(), "one vector per input");
        for v in &out {
            assert_eq!(v.len(), config.embedding_dim());
        }
    }
}
