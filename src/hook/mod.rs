use std::error::Error;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use serde::Deserialize;

use crate::config::Config;
use crate::error::VaultError;
use crate::retrieve::{Retrieval, SkipReason};
use crate::vault::{RetrieveTimings, Vault};

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
    // `pipeline` hands back the vault directory it resolved, so the log lands
    // beside the database rather than wherever `$HOME` happens to point. `None`
    // means config loading failed — the logger falls back, because that failure
    // is itself worth recording.
    let (outcome, vault_dir) = pipeline(&stdin_buf, &mut tel);

    if let Outcome::Injected { block, .. } = &outcome {
        print!("{block}");
    }
    if let Outcome::Failed { stage, detail } = &outcome {
        eprintln!(
            "vault hook: {} failed: {detail} — passthrough; see ~/.vault/hook.log",
            stage.as_str()
        );
    }
    log::append_best_effort(&outcome, &tel, started.elapsed(), vault_dir.as_deref());
    std::process::exit(0);
}

/// The UserPromptSubmit envelope sent by Claude Code. Only `prompt` is used;
/// the other documented fields (`session_id`, `transcript_path`, `cwd`,
/// `permission_mode`, `hook_event_name`) are ignored via serde's default
/// "unknown fields are skipped" behavior.
#[derive(Deserialize)]
struct HookInput {
    prompt: String,
    /// The directory Claude Code was invoked from. Optional so a client that
    /// omits it — or an older one — still parses; the cwd bias simply does not
    /// apply. See `QueryPlan::prefer_project` (review C1).
    #[serde(default)]
    cwd: Option<String>,
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

    /// Turn a library error into a hook outcome. The `Stage` comes from the
    /// error variant rather than from the call site, so telemetry cannot drift
    /// out of step with where the failure actually happened.
    ///
    /// The detail is taken from the *source*, not from `VaultError`'s own
    /// `Display`. A record already carries `stage`, so logging the wrapper text
    /// would say it twice — "router-build failed: router construction failed:
    /// ...". The wrapper prefix earns its place for a library caller who has no
    /// separate stage field; here it is noise.
    fn from_vault_error(err: &VaultError) -> Self {
        let detail = err
            .source()
            .map(|source| source.to_string())
            .unwrap_or_else(|| err.to_string());
        Outcome::Failed {
            stage: Stage::of(err),
            detail: log::truncate_detail(&detail),
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
    /// Unreachable from the hook — `vault hook` never indexes. It exists so
    /// `Stage::of` stays total without mislabelling a sync failure as a query
    /// one; an unused-but-truthful name beats a reachable-but-wrong one.
    ///
    /// Unreachable is not untested: `every_variant` covers it, so the "sync"
    /// string is pinned like every other stage. It previously was not, which
    /// meant this could have been remapped to `Stage::Query` with the whole
    /// suite still green.
    Sync,
}

impl Stage {
    /// The pipeline position a library error came from.
    ///
    /// `Stdin` has no `VaultError` counterpart on purpose: parsing the
    /// UserPromptSubmit envelope is hook-protocol work, not library work.
    pub(crate) fn of(err: &VaultError) -> Self {
        match err {
            VaultError::Config(_) => Stage::Config,
            VaultError::RouterBuild(_) => Stage::RouterBuild,
            VaultError::EmbedderBuild(_) => Stage::EmbedderBuild,
            VaultError::DbOpen(_) => Stage::DbOpen,
            VaultError::RouterPlan(_) => Stage::RouterPlan,
            VaultError::EmbedQuery(_) => Stage::EmbedQuery,
            VaultError::Query(_) => Stage::Query,
            VaultError::Sync(_) => Stage::Sync,
        }
    }

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
            Stage::Sync => "sync",
        }
    }
}

/// Returns the outcome plus the vault directory the run resolved, if it got
/// far enough to load a config. The directory is the logger's destination — see
/// `log::log_dir` for why it is an `Option` rather than always resolved here.
fn pipeline(stdin: &str, tel: &mut log::Telemetry) -> (Outcome, Option<PathBuf>) {
    let event: HookInput = match serde_json::from_str(stdin) {
        Ok(ev) => ev,
        Err(e) => return (Outcome::failed(Stage::Stdin, e), None),
    };
    // Kept even though `Vault::retrieve` now guards too: this fires before
    // `Config::load` and `Vault::open`, which build a router and a TEI client.
    // `trim()` matches the library predicate — a whitespace-only prompt used to
    // reach the router and get billed for it.
    if event.prompt.trim().is_empty() {
        return (
            Outcome::Skip {
                reason: SkipReason::EmptyPrompt,
            },
            None,
        );
    }
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return (Outcome::from_vault_error(&VaultError::Config(e)), None),
    };
    // Resolved before `Vault::open`, so a store or router failure still logs to
    // the right place.
    let vault_dir = config.vault_dir().ok();
    let vault = match Vault::open(&config) {
        Ok(v) => v,
        Err(e) => return (Outcome::from_vault_error(&e), vault_dir),
    };
    (
        pipeline_with(&event.prompt, event.cwd.as_deref(), &vault, tel),
        vault_dir,
    )
}

/// Inner pipeline with an injected `Vault` — testable with stub backends.
/// Adapts the library-shaped `retrieve_with` to the hook's fail-open `Outcome`.
fn pipeline_with(
    prompt: &str,
    cwd: Option<&str>,
    vault: &Vault,
    tel: &mut log::Telemetry,
) -> Outcome {
    match retrieve_with(prompt, cwd, vault, tel) {
        Ok(Retrieval::Skip(reason)) => Outcome::Skip { reason },
        Ok(Retrieval::Context(context)) => Outcome::Injected {
            chunks: context.hits.len(),
            tokens: context.tokens,
            block: context.render_block(),
        },
        Err(e) => Outcome::from_vault_error(&e),
    }
}

/// The retrieval pipeline in library terms: `Ok` is a decision the system made
/// (skip, or here is the context), `Err` is a failure carrying its real source.
/// Nothing here decides to fail open — that is `pipeline_with`'s job above.
///
/// Uses the planner's two steps rather than `QueryPlanner::plan` so router and
/// embed latency stay separately recorded; `hook.log` has always carried them as
/// distinct fields. Each `tel` write happens before the corresponding `?` so a
/// failure still reports the timing that preceded it.
/// The hook's retrieval call: [`Vault::retrieve_in_timed`], with the phase
/// times copied into telemetry.
///
/// This used to be a second copy of the pipeline, because the hook needs each
/// phase timed and `QueryPlanner::plan` offers no seam between routing and
/// embedding. The seam now lives on the facade, so there is one pipeline and
/// one place the "cwd is a hint, never a filter" invariant is written down.
fn retrieve_with(
    prompt: &str,
    cwd: Option<&str>,
    vault: &Vault,
    tel: &mut log::Telemetry,
) -> Result<Retrieval, VaultError> {
    let mut timings = RetrieveTimings::default();
    let result = vault.retrieve_in_timed(prompt, cwd, &mut timings);

    // Copied whatever the outcome: a failed call still spent whatever time the
    // phases before it took, and that is the number worth logging.
    tel.router_ms = timings.router_ms;
    tel.embed_ms = timings.embed_ms;
    tel.query_ms = timings.query_ms;

    // `backend` names who served the call, so it is recorded only once the
    // router phase actually ran. A blank prompt is turned away before that, and
    // stamping a backend on it would claim work nothing did. A router that ran
    // and *failed* still gets named — `router_ms` is stamped before the error
    // is propagated, which is the point.
    if timings.router_ms.is_some() {
        tel.backend = Some(vault.planner().backend());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // `RouterError` and `StoreError` are already imported by the fixtures below.
    use crate::config::ConfigError;
    use crate::embed::EmbedError;
    use crate::index::sync::SyncError;
    use crate::retrieve::Context;

    /// One instance of every `VaultError` variant, paired with the `stage`
    /// string it must produce in hook.log.
    fn every_variant() -> Vec<(VaultError, &'static str)> {
        vec![
            (VaultError::Config(ConfigError::HomeNotFound), "config"),
            (
                VaultError::RouterBuild(RouterError::Misconfigured("m".into())),
                "router-build",
            ),
            (
                VaultError::EmbedderBuild(EmbedError::Transport("t".into())),
                "embedder-build",
            ),
            (
                VaultError::DbOpen(StoreError::Backend("b".into())),
                "db-open",
            ),
            (
                VaultError::RouterPlan(RouterError::Transport("t".into())),
                "router-plan",
            ),
            (
                VaultError::EmbedQuery(EmbedError::Transport("t".into())),
                "embed-query",
            ),
            (VaultError::Query(StoreError::Backend("b".into())), "query"),
            // Unreachable from the hook — `vault hook` never indexes — but the
            // mapping ships, so its telemetry string is pinned like the rest.
            // Leaving it out is what let `Stage::Sync` carry an unasserted
            // string that could have been remapped to `Stage::Query` with no
            // test failing (review finding 15).
            (VaultError::Sync(SyncError::DeclinedRemoteCost), "sync"),
        ]
    }

    /// Ties the fixture above to the enum, which nothing else does.
    ///
    /// A new `VaultError` variant already breaks `Stage::of` — that match is
    /// exhaustive — but nothing forces `every_variant` to grow with it, which is
    /// exactly how `Sync` came to be missing while the doc comment claimed
    /// otherwise. This match breaks on a new variant *and* the count below has
    /// to be edited, so the two cannot drift apart silently again.
    #[test]
    fn every_variant_lists_all_of_them() {
        fn _exhaustive(err: &VaultError) {
            match err {
                VaultError::Config(_)
                | VaultError::RouterBuild(_)
                | VaultError::EmbedderBuild(_)
                | VaultError::DbOpen(_)
                | VaultError::RouterPlan(_)
                | VaultError::EmbedQuery(_)
                | VaultError::Query(_)
                | VaultError::Sync(_) => {}
            }
        }
        const VARIANTS: usize = 8;

        assert_eq!(
            every_variant().len(),
            VARIANTS,
            "a VaultError variant was added without a fixture — see `_exhaustive` above"
        );
    }

    /// `stage` in hook.log is a stable telemetry contract, and moving the
    /// mapping off the call sites is only safe if the strings are pinned.
    ///
    /// Adding a `VaultError` variant without extending `Stage::of` fails to
    /// compile (the match is exhaustive); changing a string fails here.
    #[test]
    fn stage_of_maps_every_variant_to_its_telemetry_string() {
        for (err, expected) in every_variant() {
            assert_eq!(Stage::of(&err).as_str(), expected, "wrong stage for {err}");
        }
    }

    /// The stage must come from the error, and the detail must still carry the
    /// underlying message — that pairing is what hook.log records.
    #[test]
    fn from_vault_error_tags_the_stage_and_keeps_the_source_text() {
        let err = VaultError::RouterPlan(RouterError::Transport("boom".into()));
        match Outcome::from_vault_error(&err) {
            Outcome::Failed { stage, detail } => {
                assert_eq!(stage, Stage::RouterPlan);
                assert!(detail.contains("boom"), "detail lost the source: {detail}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The record carries `stage` separately, so the detail must not repeat it.
    /// Regression guard: wrapping the per-layer errors in `VaultError` made the
    /// naive `err.to_string()` read "router-build failed: router construction
    /// failed: ...".
    #[test]
    fn detail_does_not_repeat_what_the_stage_already_says() {
        let err = VaultError::RouterBuild(RouterError::MissingApiKey {
            env_var: "ANTHROPIC_API_KEY".into(),
        });
        match Outcome::from_vault_error(&err) {
            Outcome::Failed { stage, detail } => {
                assert_eq!(stage, Stage::RouterBuild);
                assert!(
                    detail.starts_with("ANTHROPIC_API_KEY"),
                    "detail should be the source message, got: {detail}"
                );
                assert!(
                    !detail.contains("router construction failed"),
                    "detail duplicates the stage: {detail}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Every variant survives the round trip to an `Outcome`. Guards against a
    /// future variant that maps to a stage but loses its message.
    #[test]
    fn every_variant_round_trips_to_a_failed_outcome() {
        for (err, expected) in every_variant() {
            match Outcome::from_vault_error(&err) {
                Outcome::Failed { stage, detail } => {
                    assert_eq!(stage.as_str(), expected);
                    assert!(!detail.is_empty(), "empty detail for {expected}");
                }
                other => panic!("expected Failed for {expected}, got {other:?}"),
            }
        }
    }
    use crate::embed::{Embedder, StubEmbedder};
    use crate::retrieve::{QueryPlan, RouterError, StubRouter};
    use crate::retrieve::{Router, RouterOutput};
    use crate::store::Store;
    use crate::store::{ChunkWithEmbedding, Document, Hit, RetrievalLogEntry, StoreError};
    use crate::types::{DocType, Inventory};
    use crate::vault::{QueryPlanner, VaultStore};

    /// Fake store that returns a canned list of hits regardless of query —
    /// keeps the pipeline tests focused on hook logic, not SQL behavior.
    struct StubStore {
        hits: Vec<Hit>,
        /// Domain returned by `resolve_domain`; `None` exercises the
        /// `defaults.context_tag` fallback (the default for most pipeline tests).
        domain: Option<String>,
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
        fn resolve_domain(&self, _project_names: &[String]) -> Result<Option<String>, StoreError> {
            Ok(self.domain.clone())
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

    /// `cwd` is optional on the wire: a client that omits it must still parse,
    /// and simply contributes no bias.
    #[test]
    fn hook_input_parses_with_and_without_cwd() {
        let with: HookInput =
            serde_json::from_str(r#"{"prompt":"q","cwd":"/home/u/git/vault"}"#).expect("with cwd");
        assert_eq!(with.cwd.as_deref(), Some("/home/u/git/vault"));

        let without: HookInput = serde_json::from_str(r#"{"prompt":"q"}"#).expect("without cwd");
        assert_eq!(without.cwd, None);

        // Unknown fields Claude Code may add must not fail the parse — the hook
        // fails open, but a parse error is a `Stage::Stdin` failure that
        // suppresses context for every prompt until someone reads hook.log.
        let extra: HookInput =
            serde_json::from_str(r#"{"prompt":"q","cwd":"/x","session_id":"abc"}"#)
                .expect("extra fields");
        assert_eq!(extra.cwd.as_deref(), Some("/x"));
    }

    /// A store that records the plan it was asked to search, so the cwd bias
    /// can be observed where it actually matters. The recording handle is
    /// shared, because `vault_of` takes the store by value.
    struct RecordingStore {
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Store for RecordingStore {
        fn migrate(&mut self) -> Result<(), StoreError> {
            Ok(())
        }
        fn get_or_create_project(&mut self, _n: &str, _r: &str) -> Result<i64, StoreError> {
            Ok(1)
        }
        fn get_document_content_hash(
            &self,
            _p: i64,
            _s: &str,
        ) -> Result<Option<String>, StoreError> {
            Ok(None)
        }
        fn project_for_path(&self, path: &str) -> Result<Option<String>, StoreError> {
            Ok((path == "/home/u/git/vault/src").then(|| "vault".to_string()))
        }
        fn upsert_document(
            &mut self,
            _d: &Document,
            _c: &[ChunkWithEmbedding],
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn prune_orphans(&mut self, _p: i64, _k: &[String]) -> Result<usize, StoreError> {
            Ok(0)
        }
        fn bm25_search(&self, plan: &QueryPlan, _k: usize) -> Result<Vec<Hit>, StoreError> {
            *self.seen.lock().unwrap() = plan.projects.clone();
            Ok(Vec::new())
        }
        fn cosine_search(
            &self,
            _p: &QueryPlan,
            _e: &[f32],
            _k: usize,
        ) -> Result<Vec<Hit>, StoreError> {
            Ok(Vec::new())
        }
        fn log_retrieval(&mut self, _e: &RetrievalLogEntry) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn projects_seen_for(cwd: Option<&str>) -> Vec<String> {
        let config = Config::default();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let vault = vault_of(
            &config,
            Box::new(StubRouter),
            Box::new(StubEmbedder::from_config(&config)),
            Box::new(RecordingStore {
                seen: std::sync::Arc::clone(&seen),
            }),
        );
        let mut tel = log::Telemetry::default();
        let _ = pipeline_with("q", cwd, &vault, &mut tel);
        let guard = seen.lock().unwrap();
        guard.clone()
    }

    /// End-to-end: `cwd` must **not** narrow the search.
    ///
    /// This test previously asserted the opposite, and the PR-13 review caught
    /// it locking the bug in. `StubRouter` returns an empty `projects`, which
    /// `build_filter_clause` reads as "search every project" — the common case,
    /// since `ROUTER_SYSTEM` tells the model to omit `projects` unless the
    /// prompt names one. Putting cwd's project there converted no-filter into a
    /// single-project filter and silently excluded every other indexed repo.
    #[test]
    fn cwd_does_not_narrow_the_plan_the_store_receives() {
        assert!(
            projects_seen_for(Some("/home/u/git/vault/src")).is_empty(),
            "cwd must not add a project filter the router did not ask for"
        );
    }

    /// The three ways cwd contributes nothing must all leave the plan alone
    /// rather than failing the retrieval — it is a hint, not a requirement.
    #[test]
    fn an_unresolvable_cwd_leaves_the_plan_untouched() {
        for cwd in [None, Some("/tmp/not-indexed"), Some("")] {
            assert!(
                projects_seen_for(cwd).is_empty(),
                "cwd {cwd:?} must not bias the filter"
            );
        }
    }

    /// Assemble a `Vault` from stub backends.    /// Assemble a `Vault` from stub backends. The facade is what production
    /// uses, so the pipeline tests go through it too rather than around it.
    fn vault_of(
        config: &Config,
        router: Box<dyn Router + Send + Sync>,
        embedder: Box<dyn Embedder + Send + Sync>,
        store: Box<dyn Store + Send>,
    ) -> Vault {
        Vault::from_parts(
            QueryPlanner::from_parts(router, embedder, Inventory::default()),
            VaultStore::from_store(store, config.clone()),
        )
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
        fn plan(&self, _prompt: &str, _inventory: &Inventory) -> Result<RouterOutput, RouterError> {
            Ok(RouterOutput::Skip)
        }
    }

    /// Router that always fails — exercises the `Failed(RouterPlan)` path.
    struct ErrRouter;
    impl Router for ErrRouter {
        fn name(&self) -> &'static str {
            "err-stub"
        }
        fn plan(&self, _prompt: &str, _inventory: &Inventory) -> Result<RouterOutput, RouterError> {
            Err(RouterError::Transport("connection refused".into()))
        }
    }

    #[test]
    fn pipeline_injects_block_when_hits_returned() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![sample_hit("BuildRequest", "message BuildRequest {}", 0.9)],
            domain: None,
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "what is BuildRequest?",
            None,
            &vault_of(
                &config,
                Box::new(StubRouter),
                Box::new(embedder),
                Box::new(store),
            ),
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
    fn pipeline_uses_domain_context_tag_when_project_assigned() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![sample_hit("BuildRequest", "message BuildRequest {}", 0.9)],
            domain: Some("finance".to_string()),
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "q",
            None,
            &vault_of(
                &config,
                Box::new(StubRouter),
                Box::new(embedder),
                Box::new(store),
            ),
            &mut tel,
        );
        let Outcome::Injected { block, .. } = out else {
            panic!("expected Injected, got {out:?}");
        };
        // The tag is constant; the assigned domain rides as an attribute, so a
        // new domain needs no new `## {domain}-context` section in
        // ~/.claude/CLAUDE.md to stay framed.
        assert!(block.starts_with("<vault-context domain=\"finance\">\n"));
        assert!(block.ends_with("</vault-context>\n"));
        // The closing tag carries no attribute — it would not be well-formed.
        assert!(!block.contains("</vault-context domain"));
    }

    #[test]
    fn pipeline_records_per_stage_telemetry_on_success() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![sample_hit("A", "alpha", 0.9)],
            domain: None,
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let _ = pipeline_with(
            "q",
            None,
            &vault_of(
                &config,
                Box::new(StubRouter),
                Box::new(embedder),
                Box::new(store),
            ),
            &mut tel,
        );
        assert_eq!(tel.backend, Some("stub"));
        assert!(tel.router_ms.is_some());
        assert!(tel.embed_ms.is_some());
        assert!(tel.query_ms.is_some());
    }

    #[test]
    fn pipeline_skips_when_router_says_skip() {
        let config = Config::default();
        let store = StubStore {
            hits: vec![],
            domain: None,
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "hi",
            None,
            &vault_of(
                &config,
                Box::new(SkipRouter),
                Box::new(embedder),
                Box::new(store),
            ),
            &mut tel,
        );
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
        let store = StubStore {
            hits: vec![],
            domain: None,
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "q",
            None,
            &vault_of(
                &config,
                Box::new(ErrRouter),
                Box::new(embedder),
                Box::new(store),
            ),
            &mut tel,
        );
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
        let store = StubStore {
            hits: vec![],
            domain: None,
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "anything",
            None,
            &vault_of(
                &config,
                Box::new(StubRouter),
                Box::new(embedder),
                Box::new(store),
            ),
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
            domain: None,
        };
        let embedder = StubEmbedder::from_config(&config);
        let mut tel = log::Telemetry::default();
        let out = pipeline_with(
            "x",
            None,
            &vault_of(
                &config,
                Box::new(StubRouter),
                Box::new(embedder),
                Box::new(store),
            ),
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
    fn pipeline_top_level_fails_stdin_stage_on_malformed_input() {
        // Malformed input is a Failed outcome (Claude Code should always send
        // a valid envelope), distinguished from deliberate skips.
        for bad in ["not json at all", "", "{}"] {
            let mut tel = log::Telemetry::default();
            let (out, dir) = pipeline(bad, &mut tel);
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
            // Bailed before a config existed; the logger falls back to $HOME.
            assert!(
                dir.is_none(),
                "input {bad:?} resolved a dir it never loaded"
            );
        }
    }

    #[test]
    fn pipeline_top_level_skips_on_empty_prompt() {
        // Valid envelope, empty prompt body — bails before touching any
        // backend (no Config load, no router probe).
        let mut tel = log::Telemetry::default();
        let (out, dir) = pipeline(r#"{"prompt": ""}"#, &mut tel);
        assert!(matches!(
            out,
            Outcome::Skip {
                reason: SkipReason::EmptyPrompt
            }
        ));
        assert!(
            dir.is_none(),
            "no config is loaded on the empty-prompt path"
        );
    }

    /// The guard in `pipeline` covers the real hook, but `pipeline_with` is the
    /// seam a test — or a future in-process caller — drives directly, and it
    /// used to disagree with `Vault::retrieve` about what a blank prompt is.
    ///
    /// `QueryPlanner::route` returns `Ok(None)` for both "prompt was blank" and
    /// "router saw no need for context", so without its own guard
    /// `retrieve_with` reported every blank prompt as a `RouterSkip`. The two
    /// call for different follow-up: one is a caller bug, the other is the
    /// system working.
    #[test]
    fn pipeline_with_reports_a_blank_prompt_as_empty_not_a_router_skip() {
        let config = Config::default();
        for prompt in ["", "   ", "\n\t "] {
            let embedder = StubEmbedder::from_config(&config);
            let store = StubStore {
                hits: vec![sample_hit("BuildRequest", "message BuildRequest {}", 0.9)],
                domain: None,
            };
            let mut tel = log::Telemetry::default();
            let out = pipeline_with(
                prompt,
                None,
                &vault_of(
                    &config,
                    Box::new(StubRouter),
                    Box::new(embedder),
                    Box::new(store),
                ),
                &mut tel,
            );
            assert!(
                matches!(
                    out,
                    Outcome::Skip {
                        reason: SkipReason::EmptyPrompt
                    }
                ),
                "expected EmptyPrompt for {prompt:?}, got {out:?}"
            );
            // The store here would happily return a hit, so a wrong answer
            // shows up as `Injected` rather than a subtler mislabel.
            assert!(
                tel.backend.is_none() && tel.router_ms.is_none(),
                "a blank prompt must not reach the planner at all"
            );
        }
    }

    /// `render_block` moved onto `Context`; these tests still assert the exact
    /// bytes `vault hook` writes to stdout, which is the contract that matters.
    fn block(tag: &str, hits: Vec<Hit>) -> String {
        block_in(tag, None, hits)
    }

    fn block_in(tag: &str, domain: Option<&str>, hits: Vec<Hit>) -> String {
        Context {
            tag: tag.to_string(),
            domain: domain.map(str::to_string),
            hits,
            tokens: 0,
        }
        .render_block()
    }

    #[test]
    fn render_block_single_chunk_shape() {
        let chunks = vec![sample_hit("Foo", "body line 1", 0.5)];
        let out = block("tag-x", chunks);
        assert_eq!(out, "<tag-x>\n## Foo [contract]\nbody line 1\n</tag-x>\n");
    }

    /// An assigned domain becomes an attribute on the opening tag only.
    #[test]
    fn render_block_carries_the_domain_as_an_attribute() {
        let chunks = vec![sample_hit("Foo", "body line 1", 0.5)];
        let out = block_in("tag-x", Some("software"), chunks);
        assert_eq!(
            out,
            "<tag-x domain=\"software\">\n## Foo [contract]\nbody line 1\n</tag-x>\n"
        );
    }

    /// No domain means no attribute — not a placeholder value. Absence of the
    /// attribute is the encoding of absence; a placeholder would assert
    /// something, and could collide with a real domain of the same name.
    #[test]
    fn render_block_omits_the_attribute_when_no_domain_is_assigned() {
        let out = block_in("tag-x", None, vec![sample_hit("Foo", "b", 0.5)]);
        assert!(out.starts_with("<tag-x>\n"), "got {out:?}");
        assert!(!out.contains("domain="));
    }

    /// A hostile domain drops the attribute rather than substituting a
    /// placeholder: the block stays framed, and vault does not report "no
    /// domain" for what was actually an unusable one.
    #[test]
    fn render_block_drops_a_hostile_domain_without_claiming_unassigned() {
        let out = block_in(
            "tag-x",
            Some("\"><script>alert(1)</script"),
            vec![sample_hit("Foo", "b", 0.5)],
        );
        assert!(out.starts_with("<tag-x>\n"), "got {out:?}");
        assert!(!out.contains("script"));
        assert!(!out.contains("unassigned"));
    }

    #[test]
    fn render_block_multiple_chunks_separated_by_blank_line() {
        let chunks = vec![sample_hit("A", "alpha", 0.9), sample_hit("B", "beta", 0.8)];
        let out = block("ctx", chunks);
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
        let out = block("t", chunks);
        let first_pos = out.find("first").unwrap();
        let second_pos = out.find("second").unwrap();
        let third_pos = out.find("third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }
}
