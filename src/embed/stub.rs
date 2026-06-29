use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::config::Config;
use crate::embed::{EmbedError, Embedder};

pub struct StubEmbedder {
    dim: usize,
}

impl StubEmbedder {
    pub fn from_config(config: &Config) -> Self {
        Self {
            dim: config.embedding_dim(),
        }
    }
}

impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_document(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(deterministic_unit_vector(
            &format!("search_document: {text}"),
            self.dim,
        ))
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(deterministic_unit_vector(
            &format!("search_query: {text}"),
            self.dim,
        ))
    }
}

fn deterministic_unit_vector(text: &str, dim: usize) -> Vec<f32> {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    let idx = (h.finish() as usize) % dim;
    let mut v = vec![0.0; dim];
    v[idx] = 1.0;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_input_yields_same_vector() {
        let e = StubEmbedder::from_config(&Config::default());
        assert_eq!(
            e.embed_document("foo").unwrap(),
            e.embed_document("foo").unwrap()
        );
    }

    #[test]
    fn document_and_query_prefixes_differ() {
        let e = StubEmbedder::from_config(&Config::default());
        assert_ne!(
            e.embed_document("foo").unwrap(),
            e.embed_query("foo").unwrap()
        );
    }

    #[test]
    fn returns_unit_vector_at_configured_dim() {
        let config = Config::default();
        let e = StubEmbedder::from_config(&config);
        let v = e.embed_query("anything").unwrap();
        assert_eq!(v.len(), config.embedding_dim());
        assert_eq!(v.iter().filter(|f| **f != 0.0).count(), 1);
        assert!(v.contains(&1.0));
    }

    // The default `embed_documents` (used by every backend without a batch
    // endpoint) must return one vector per input, in order, identical to calling
    // `embed_document` individually. sync zips the result against its chunks, so
    // order and count are load-bearing.
    #[test]
    fn default_embed_documents_matches_individual_in_order() {
        let e = StubEmbedder::from_config(&Config::default());
        let texts = ["alpha", "beta", "gamma"];
        let batch = e.embed_documents(&texts).unwrap();
        assert_eq!(batch.len(), 3);
        for (i, t) in texts.iter().enumerate() {
            assert_eq!(batch[i], e.embed_document(t).unwrap());
        }
    }

    #[test]
    fn default_embed_documents_empty_input_returns_empty() {
        let e = StubEmbedder::from_config(&Config::default());
        assert!(e.embed_documents(&[]).unwrap().is_empty());
    }
}
