use std::io::Read;

use serde::Deserialize;

use crate::config::Config;
use crate::embed::{Embedder, TeiEmbedder};
use crate::retrieve::{self, RouterOutput, Router, budget};
use crate::store::{Hit, SqliteStore, Store};

/// Entry for `vault hook`. Reads a UserPromptSubmit envelope on stdin, runs
/// the retrieval pipeline, and prints the rendered context block on stdout.
/// **Always exits 0** — Claude Code appends our stdout to the prompt context,
/// so an empty stdout is the silent-passthrough signal. Exiting non-zero (or
/// emitting on stderr alone) would surface as an error in Claude Code's
/// transcript; exit 2 would erase the user's prompt outright. Fail open.
pub fn run() -> ! {
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    if let Some(block) = pipeline(&stdin_buf) {
        print!("{block}");
    }
    std::process::exit(0);
}

/// The UserPromptSubmit envelope sent by Claude Code. Only `prompt` is used;
/// the other documented fields (`session_id`, `transcript_path`, `cwd`,
/// `permission_mode`, `hook_event_name`) are ignored via serde's default
/// "unknown fields are skipped" behavior.
#[derive(Deserialize)]
struct HookInput {
    prompt: String,
}

fn pipeline(stdin: &str) -> Option<String> {
    let event: HookInput = serde_json::from_str(stdin).ok()?;
    if event.prompt.is_empty() {
        return None;
    }
    let config = Config::load().ok()?;
    let router = retrieve::build_router(&config).ok()?;
    let embedder = TeiEmbedder::from_config(&config).ok()?;
    let db_path = config.db_path().ok()?;
    let store = SqliteStore::open(&db_path, &config).ok()?;
    pipeline_with(&event.prompt, &config, &*router, &embedder, &store)
}

/// Inner pipeline with injected dependencies — testable with stubs. Returns
/// `None` for any of: router skip, empty selection after budget, or any
/// downstream error (the outer `run` swallows all of these as passthrough).
fn pipeline_with(
    prompt: &str,
    config: &Config,
    router: &dyn Router,
    embedder: &dyn Embedder,
    store: &dyn Store,
) -> Option<String> {
    let plan = match router.plan(prompt).ok()? {
        RouterOutput::Skip => return None,
        RouterOutput::Plan(p) => p,
    };
    let emb = embedder.embed_query(prompt).ok()?;
    let hits = store.hybrid_search(&plan, &emb, config.alpha()).ok()?;
    let sel = budget::select_within_budget(
        hits,
        config.token_budget() as u32,
        config.min_score(),
    );
    if sel.chunks.is_empty() {
        return None;
    }
    let tag = config.resolve_context_tag(&plan.projects);
    Some(render_block(tag, &sel.chunks))
}

/// Render the context block Claude Code will see appended to the user's
/// prompt. Each chunk gets a `## label [doc_type]` header so Claude can tell
/// the sources apart; chunks arrive in score-descending order (preserved by
/// `select_within_budget`).
fn render_block(tag: &str, chunks: &[Hit]) -> String {
    let mut out = String::new();
    out.push('<');
    out.push_str(tag);
    out.push_str(">\n");
    for (i, c) in chunks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("## ");
        out.push_str(&c.label);
        out.push_str(" [");
        out.push_str(c.doc_type.as_str());
        out.push_str("]\n");
        out.push_str(&c.content);
        out.push('\n');
    }
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::StubEmbedder;
    use crate::retrieve::{QueryPlan, StubRouter};
    use crate::store::{ChunkWithEmbedding, Document, RetrievalLogEntry, StoreError};
    use crate::types::DocType;

    /// Fake store that returns a canned list of hits regardless of query —
    /// keeps the pipeline tests focused on hook logic, not SQL behavior.
    struct StubStore {
        hits: Vec<Hit>,
    }

    impl Store for StubStore {
        fn migrate(&mut self) -> Result<(), StoreError> {
            Ok(())
        }
        fn upsert_document(
            &mut self,
            _doc: &Document,
            _chunks: &[ChunkWithEmbedding],
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn prune_orphans(
            &mut self,
            _project_id: i64,
            _kept_paths: &[String],
        ) -> Result<usize, StoreError> {
            Ok(0)
        }
        fn hybrid_search(
            &self,
            _plan: &QueryPlan,
            _embedding: &[f32],
            _alpha: f32,
        ) -> Result<Vec<Hit>, StoreError> {
            Ok(self.hits.clone())
        }
        fn log_retrieval(&mut self, _entry: &RetrievalLogEntry) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn sample_hit(label: &str, content: &str, score: f32) -> Hit {
        Hit {
            chunk_id: 1,
            project_id: 1,
            doc_type: DocType::Contract,
            label: label.to_string(),
            content: content.to_string(),
            token_est: 50,
            bm25_score: 0.0,
            cosine_score: 0.0,
            final_score: score,
        }
    }

    struct SkipRouter;
    impl Router for SkipRouter {
        fn plan(&self, _prompt: &str) -> Result<RouterOutput, retrieve::RouterError> {
            Ok(RouterOutput::Skip)
        }
    }

    #[test]
    fn pipeline_renders_block_when_hits_returned() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![sample_hit("BuildRequest", "message BuildRequest {}", 0.9)],
        };
        let embedder = StubEmbedder::from_config(&config);
        let out = pipeline_with("what is BuildRequest?", &config, &StubRouter, &embedder, &store)
            .expect("expected block");
        assert!(out.starts_with("<vault-context>\n"));
        assert!(out.contains("## BuildRequest [contract]"));
        assert!(out.contains("message BuildRequest {}"));
        assert!(out.ends_with("</vault-context>\n"));
    }

    #[test]
    fn pipeline_returns_none_on_skip() {
        let config = Config::default();
        let store = StubStore { hits: vec![] };
        let embedder = StubEmbedder::from_config(&config);
        let out = pipeline_with("hi", &config, &SkipRouter, &embedder, &store);
        assert!(out.is_none());
    }

    #[test]
    fn pipeline_returns_none_when_hits_empty() {
        let config = Config::default();
        let store = StubStore { hits: vec![] };
        let embedder = StubEmbedder::from_config(&config);
        let out = pipeline_with("anything", &config, &StubRouter, &embedder, &store);
        assert!(out.is_none());
    }

    #[test]
    fn pipeline_returns_none_when_min_score_filters_everything() {
        let config = Config::default();
        // Hit below the default min_score=0.15 — budget gate drops it, leaving
        // an empty selection.
        let store = StubStore {
            hits: vec![sample_hit("low", "noise", 0.05)],
        };
        let embedder = StubEmbedder::from_config(&config);
        let out = pipeline_with("x", &config, &StubRouter, &embedder, &store);
        assert!(out.is_none());
    }

    #[test]
    fn pipeline_top_level_handles_malformed_stdin() {
        // Outer `pipeline` reads from Config::load and lots else — we only need
        // to prove malformed JSON bails out cleanly to None.
        assert!(pipeline("not json at all").is_none());
        assert!(pipeline("").is_none());
        assert!(pipeline("{}").is_none()); // missing prompt field → deser fails
    }

    #[test]
    fn pipeline_top_level_short_circuits_on_empty_prompt() {
        // Valid envelope, empty prompt body — should bail before touching any
        // backend. Won't try to load Config because we short-circuit first.
        let stdin = r#"{"prompt": ""}"#;
        assert!(pipeline(stdin).is_none());
    }

    #[test]
    fn render_block_single_chunk_shape() {
        let chunks = vec![sample_hit("Foo", "body line 1", 0.5)];
        let out = render_block("tag-x", &chunks);
        assert_eq!(out, "<tag-x>\n## Foo [contract]\nbody line 1\n</tag-x>\n");
    }

    #[test]
    fn render_block_multiple_chunks_separated_by_blank_line() {
        let chunks = vec![
            sample_hit("A", "alpha", 0.9),
            sample_hit("B", "beta", 0.8),
        ];
        let out = render_block("ctx", &chunks);
        // Blank line between chunks; no leading blank before the first.
        let expected = "<ctx>\n## A [contract]\nalpha\n\n## B [contract]\nbeta\n</ctx>\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_block_preserves_input_order() {
        // select_within_budget passes input order through; render must too.
        let chunks = vec![
            sample_hit("first", "1", 0.5),
            sample_hit("second", "2", 0.9),
            sample_hit("third", "3", 0.7),
        ];
        let out = render_block("t", &chunks);
        let first_pos = out.find("first").unwrap();
        let second_pos = out.find("second").unwrap();
        let third_pos = out.find("third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }

}
