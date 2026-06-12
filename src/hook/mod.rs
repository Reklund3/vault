use std::io::Read;
use std::time::Instant;

use serde::Deserialize;

use crate::config::Config;
use crate::embed::{Embedder, TeiEmbedder};
use crate::retrieve::{self, Router, RouterOutput, budget};
use crate::store::{Hit, SqliteStore, Store};

mod log;

/// Entry for `vault hook`. Reads a UserPromptSubmit envelope on stdin, runs
/// the retrieval pipeline, and prints the rendered context block on stdout.
/// **Always exits 0** — Claude Code appends our stdout to the prompt context,
/// so an empty stdout is the silent-passthrough signal. Exiting non-zero (or
/// exit 2) would surface as an error or erase the user's prompt. Fail open.
///
/// Passthrough is silent to Claude Code but not to us: every invocation
/// appends one metadata-only JSONL record to `~/.vault/hook.log`, and `Failed`
/// outcomes also write a one-line stderr breadcrumb (with exit 0, Claude Code
/// shows hook stderr only in debug mode — invisible in normal use).
pub fn run() -> ! {
    let started = Instant::now();
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);

    let mut tel = log::Telemetry::default();
    let outcome = pipeline(&stdin_buf, &mut tel);

    if let Outcome::Injected { block, .. } = &outcome {
        print!("{block}");
    }
    if let Outcome::Failed { stage, detail } = &outcome {
        eprintln!(
            "vault hook: {} failed: {detail} — passthrough; see ~/.vault/hook.log",
            stage.as_str()
        );
    }
    log::append_best_effort(&outcome, &tel, started.elapsed());
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

/// Everything a hook invocation can resolve to. `Skip` and `Failed` both end
/// in passthrough (empty stdout), but they are different facts — `Skip` is the
/// system working as designed, `Failed` is infrastructure trouble — and
/// hook.log records which one happened. Collapsing both into one `None` was
/// exactly the observability hole this enum closes.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Context rendered and emitted on stdout.
    Injected {
        block: String,
        chunks: usize,
        tokens: u32,
    },
    /// Deliberate no-injection — not an error.
    Skip { reason: SkipReason },
    /// Infrastructure failure — passthrough, breadcrumb on stderr, detail in
    /// hook.log.
    Failed { stage: Stage, detail: String },
}

impl Outcome {
    fn failed(stage: Stage, err: impl std::fmt::Display) -> Self {
        Outcome::Failed {
            stage,
            detail: log::truncate_detail(&err.to_string()),
        }
    }
}

/// Why the hook deliberately injected nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Envelope parsed but the prompt body was empty.
    EmptyPrompt,
    /// The router returned `{ skip: true }` — prompt needs no context.
    RouterSkip,
    /// Retrieval ran but nothing survived min-score + budget selection.
    NoHits,
}

impl SkipReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SkipReason::EmptyPrompt => "empty-prompt",
            SkipReason::RouterSkip => "router-skip",
            SkipReason::NoHits => "no-hits",
        }
    }
}

/// Pipeline position of a failure, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Stdin,
    Config,
    RouterBuild,
    EmbedderBuild,
    DbOpen,
    RouterPlan,
    EmbedQuery,
    Query,
}

impl Stage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Stage::Stdin => "stdin",
            Stage::Config => "config",
            Stage::RouterBuild => "router-build",
            Stage::EmbedderBuild => "embedder-build",
            Stage::DbOpen => "db-open",
            Stage::RouterPlan => "router-plan",
            Stage::EmbedQuery => "embed-query",
            Stage::Query => "query",
        }
    }
}

fn ms_since(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn pipeline(stdin: &str, tel: &mut log::Telemetry) -> Outcome {
    let event: HookInput = match serde_json::from_str(stdin) {
        Ok(ev) => ev,
        Err(e) => return Outcome::failed(Stage::Stdin, e),
    };
    if event.prompt.is_empty() {
        return Outcome::Skip {
            reason: SkipReason::EmptyPrompt,
        };
    }
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return Outcome::failed(Stage::Config, e),
    };
    let router = match retrieve::build_router(&config) {
        Ok(r) => r,
        Err(e) => return Outcome::failed(Stage::RouterBuild, e),
    };
    let embedder = match TeiEmbedder::from_config(&config) {
        Ok(em) => em,
        Err(e) => return Outcome::failed(Stage::EmbedderBuild, e),
    };
    let db_path = match config.db_path() {
        Ok(p) => p,
        Err(e) => return Outcome::failed(Stage::Config, e),
    };
    let store = match SqliteStore::open(&db_path, &config) {
        Ok(s) => s,
        Err(e) => return Outcome::failed(Stage::DbOpen, e),
    };
    pipeline_with(&event.prompt, &config, &*router, &embedder, &store, tel)
}

/// Inner pipeline with injected dependencies — testable with stubs. Fills
/// `tel` with per-stage latency as it goes, so even a `Failed` record carries
/// the timing that preceded the failure (a router timeout shows up as
/// `router_ms` ≈ the configured timeout).
fn pipeline_with(
    prompt: &str,
    config: &Config,
    router: &dyn Router,
    embedder: &dyn Embedder,
    store: &dyn Store,
    tel: &mut log::Telemetry,
) -> Outcome {
    tel.backend = Some(router.name());

    let t = Instant::now();
    let planned = router.plan(prompt);
    tel.router_ms = Some(ms_since(t));
    let plan = match planned {
        Ok(RouterOutput::Skip) => {
            return Outcome::Skip {
                reason: SkipReason::RouterSkip,
            };
        }
        Ok(RouterOutput::Plan(p)) => p,
        Err(e) => return Outcome::failed(Stage::RouterPlan, e),
    };

    let t = Instant::now();
    let embedded = embedder.embed_query(prompt);
    tel.embed_ms = Some(ms_since(t));
    let emb = match embedded {
        Ok(v) => v,
        Err(e) => return Outcome::failed(Stage::EmbedQuery, e),
    };

    let t = Instant::now();
    let searched = store.hybrid_search(&plan, &emb, config.alpha());
    tel.query_ms = Some(ms_since(t));
    let hits = match searched {
        Ok(h) => h,
        Err(e) => return Outcome::failed(Stage::Query, e),
    };

    let sel = budget::select_within_budget(hits, config.token_budget() as u32, config.min_score());
    if sel.chunks.is_empty() {
        return Outcome::Skip {
            reason: SkipReason::NoHits,
        };
    }
    let tag = config.resolve_context_tag(&plan.projects);
    let chunks = sel.chunks.len();
    let tokens = sel.tokens_used;
    Outcome::Injected {
        block: render_block(tag, &sel.chunks),
        chunks,
        tokens,
    }
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
    use crate::retrieve::{QueryPlan, RouterError, StubRouter};
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
        fn get_or_create_project(
            &mut self,
            _name: &str,
            _repo_path: &str,
        ) -> Result<i64, StoreError> {
            Ok(1)
        }
        fn get_document_content_hash(
            &self,
            _project_id: i64,
            _source_path: &str,
        ) -> Result<Option<String>, StoreError> {
            Ok(None)
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
        // Required primitives — unused here because we override hybrid_search to
        // return canned hits directly (keeping these tests about hook logic, not
        // the merge, which is covered in retrieve::hybrid).
        fn bm25_search(&self, _plan: &QueryPlan, _top_k: usize) -> Result<Vec<Hit>, StoreError> {
            Ok(Vec::new())
        }
        fn cosine_search(
            &self,
            _plan: &QueryPlan,
            _embedding: &[f32],
            _top_k: usize,
        ) -> Result<Vec<Hit>, StoreError> {
            Ok(Vec::new())
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
        fn name(&self) -> &'static str {
            "skip-stub"
        }
        fn plan(&self, _prompt: &str) -> Result<RouterOutput, RouterError> {
            Ok(RouterOutput::Skip)
        }
    }

    /// Router that always fails — exercises the `Failed(RouterPlan)` path.
    struct ErrRouter;
    impl Router for ErrRouter {
        fn name(&self) -> &'static str {
            "err-stub"
        }
        fn plan(&self, _prompt: &str) -> Result<RouterOutput, RouterError> {
            Err(RouterError::Transport("connection refused".into()))
        }
    }

    #[test]
    fn pipeline_injects_block_when_hits_returned() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![sample_hit("BuildRequest", "message BuildRequest {}", 0.9)],
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "what is BuildRequest?",
            &config,
            &StubRouter,
            &embedder,
            &store,
            &mut tel,
        );
        let Outcome::Injected {
            block,
            chunks,
            tokens,
        } = out
        else {
            panic!("expected Injected, got {out:?}");
        };
        assert!(block.starts_with("<vault-context>\n"));
        assert!(block.contains("## BuildRequest [contract]"));
        assert!(block.contains("message BuildRequest {}"));
        assert!(block.ends_with("</vault-context>\n"));
        assert_eq!(chunks, 1);
        assert_eq!(tokens, 50);
    }

    #[test]
    fn pipeline_records_per_stage_telemetry_on_success() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![sample_hit("A", "alpha", 0.9)],
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let _ = pipeline_with("q", &config, &StubRouter, &embedder, &store, &mut tel);
        assert_eq!(tel.backend, Some("stub"));
        assert!(tel.router_ms.is_some());
        assert!(tel.embed_ms.is_some());
        assert!(tel.query_ms.is_some());
    }

    #[test]
    fn pipeline_skips_when_router_says_skip() {
        let config = Config::default();
        let store = StubStore { hits: vec![] };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with("hi", &config, &SkipRouter, &embedder, &store, &mut tel);
        assert!(matches!(
            out,
            Outcome::Skip {
                reason: SkipReason::RouterSkip
            }
        ));
        // Returned before embedding: router timing recorded, later stages not.
        assert!(tel.router_ms.is_some());
        assert!(tel.embed_ms.is_none());
        assert!(tel.query_ms.is_none());
    }

    #[test]
    fn pipeline_failed_router_keeps_stage_detail_and_timing() {
        let config = Config::default();
        let store = StubStore { hits: vec![] };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with("q", &config, &ErrRouter, &embedder, &store, &mut tel);
        let Outcome::Failed { stage, detail } = out else {
            panic!("expected Failed, got {out:?}");
        };
        assert_eq!(stage, Stage::RouterPlan);
        assert!(detail.contains("connection refused"), "detail: {detail}");
        assert_eq!(tel.backend, Some("err-stub"));
        assert!(tel.router_ms.is_some());
    }

    #[test]
    fn pipeline_skips_no_hits_when_store_empty() {
        let config = Config::default();
        let store = StubStore { hits: vec![] };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "anything",
            &config,
            &StubRouter,
            &embedder,
            &store,
            &mut tel,
        );
        assert!(matches!(
            out,
            Outcome::Skip {
                reason: SkipReason::NoHits
            }
        ));
    }

    #[test]
    fn pipeline_skips_no_hits_when_min_score_filters_everything() {
        let config = Config::default();
        // Hit below the default min_score=0.15 — budget gate drops it, leaving
        // an empty selection.
        let store = StubStore {
            hits: vec![sample_hit("low", "noise", 0.05)],
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with("x", &config, &StubRouter, &embedder, &store, &mut tel);
        assert!(matches!(
            out,
            Outcome::Skip {
                reason: SkipReason::NoHits
            }
        ));
    }

    #[test]
    fn pipeline_top_level_fails_stdin_stage_on_malformed_input() {
        // Malformed input is a Failed outcome (Claude Code should always send
        // a valid envelope), distinguished from deliberate skips.
        for bad in ["not json at all", "", "{}"] {
            let mut tel = log::Telemetry::default();
            let out = pipeline(bad, &mut tel);
            assert!(
                matches!(
                    out,
                    Outcome::Failed {
                        stage: Stage::Stdin,
                        ..
                    }
                ),
                "input {bad:?} → {out:?}"
            );
        }
    }

    #[test]
    fn pipeline_top_level_skips_on_empty_prompt() {
        // Valid envelope, empty prompt body — bails before touching any
        // backend (no Config load, no router probe).
        let mut tel = log::Telemetry::default();
        let out = pipeline(r#"{"prompt": ""}"#, &mut tel);
        assert!(matches!(
            out,
            Outcome::Skip {
                reason: SkipReason::EmptyPrompt
            }
        ));
    }

    #[test]
    fn render_block_single_chunk_shape() {
        let chunks = vec![sample_hit("Foo", "body line 1", 0.5)];
        let out = render_block("tag-x", &chunks);
        assert_eq!(out, "<tag-x>\n## Foo [contract]\nbody line 1\n</tag-x>\n");
    }

    #[test]
    fn render_block_multiple_chunks_separated_by_blank_line() {
        let chunks = vec![sample_hit("A", "alpha", 0.9), sample_hit("B", "beta", 0.8)];
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
