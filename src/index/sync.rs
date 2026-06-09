use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config::{Config, ConfigError};
use crate::embed::{EmbedError, Embedder, TeiEmbedder};
use crate::index::classify::{
    Classification, Classifier, ClassifyError, ClassifyInput, ResolvedBackend, build_classifier,
    cost_estimate, resolve_backend,
};
use crate::index::secrets;
use crate::index::walk::{WalkError, WalkOptions, Walked, walk_repo};
use crate::parse::{self, parser_for};
use crate::store::{Chunk, ChunkWithEmbedding, Document, SqliteStore, Store, StoreError};
use crate::types::{DocType, Language};

const HEAD_BYTES: usize = 1024;

pub struct SyncOptions {
    pub repo: PathBuf,
    pub explicit_name: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub project: String,
    pub project_id: i64, // 0 in dry-run (no store touched)
    pub dry_run: bool,
    pub files_walked: usize,
    pub files_cached: usize,                  // matched a vault.toml override
    pub files_classified: usize,              // called the live classifier (0 in dry-run)
    pub files_would_classify: usize,          // dry-run only — # that would have classified
    pub files_unchanged: usize,               // content_hash matched (0 in dry-run)
    pub files_skipped_remote_classify: usize, // head trip → ext fallback (0 in dry-run)
    pub files_parsed_via_parser: usize,       // 0 in dry-run
    pub files_parsed_as_whole: usize,         // 0 in dry-run
    pub files_skipped: Vec<(String, String)>, // (relative_path, reason)
    pub chunks_indexed: usize,                // 0 in dry-run
    pub chunks_dropped_secret: usize,         // 0 in dry-run
    pub orphans_pruned: usize,                // 0 in dry-run
    pub estimated_haiku_cost_usd: f64,        // dry-run; 0.0 if not auto→Haiku
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("project name collision for {name:?}: {message}")]
    ProjectNameCollision { name: String, message: String },
    #[error("declined Haiku cost — sync aborted")]
    DeclinedHaikuCost,
    #[error(
        "TEI embeddings server unreachable ({0}).\n\
         Start it with `vault tei start` (or check `vault tei status`), then re-run sync."
    )]
    TeiUnreachable(EmbedError),
    #[error("classifier construction failed: {0}")]
    BuildClassifier(ClassifyError),
    #[error("walk error: {0}")]
    Walk(WalkError),
    #[error("store error: {0}")]
    Store(StoreError),
    #[error("io error: {0}")]
    Io(std::io::Error),
    #[error("config error: {0}")]
    Config(ConfigError),
}

/// Top-level entry point. Builds the real services (TeiEmbedder, classifier,
/// SqliteStore) and delegates to `sync_with`. Dry-run short-circuits before any
/// remote services are touched — see `dry_run_report`.
pub fn run_sync(opts: SyncOptions, config: &Config) -> Result<SyncReport, SyncError> {
    let canonical = std::fs::canonicalize(&opts.repo).map_err(SyncError::Io)?;
    let project_name = opts
        .explicit_name
        .clone()
        .unwrap_or_else(|| derive_project_name(&canonical));

    let walked = walk_repo(
        &canonical,
        &WalkOptions {
            user_extra_excludes: config.indexer_exclude_patterns().to_vec(),
        },
    )
    .map_err(SyncError::Walk)?;

    if opts.dry_run {
        return Ok(dry_run_report(&walked, config, &canonical, project_name));
    }

    let embedder = TeiEmbedder::from_config(config).map_err(SyncError::TeiUnreachable)?;
    embedder
        .verify_against_server()
        .map_err(SyncError::TeiUnreachable)?;

    let mode = config.classifier_mode();
    let backend = resolve_backend(config);
    if mode == "auto" && backend == ResolvedBackend::Haiku && !walked.is_empty() {
        prompt_for_haiku_cost(walked.len(), std::io::stdin().lock(), std::io::stderr())?;
    }

    let classifier = build_classifier(config).map_err(SyncError::BuildClassifier)?;

    let db_path = config.db_path().map_err(SyncError::Config)?;
    let mut store = SqliteStore::open(&db_path, config).map_err(SyncError::Store)?;
    let canonical_str = canonical.to_str().unwrap_or_default().to_string();
    let project_id = store
        .get_or_create_project(&project_name, &canonical_str)
        .map_err(|e| match e {
            StoreError::Conflict(msg) => SyncError::ProjectNameCollision {
                name: project_name.clone(),
                message: msg,
            },
            other => SyncError::Store(other),
        })?;

    sync_with(
        project_id,
        project_name,
        &canonical,
        config,
        &walked,
        &mut store,
        &embedder,
        classifier.as_ref(),
    )
}

/// Core orchestrator. Takes trait objects so tests can swap stubs without
/// constructing TEI / SQLite. Caller is responsible for the cost-prompt and
/// project-id resolution.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_with(
    project_id: i64,
    project_name: String,
    canonical_repo: &Path,
    config: &Config,
    walked: &[Walked],
    store: &mut dyn Store,
    embedder: &dyn Embedder,
    classifier: &dyn Classifier,
) -> Result<SyncReport, SyncError> {
    let canonical_repo_str = canonical_repo.to_str().unwrap_or_default();

    // Populated from the walk up front. Deletion-detection is decoupled from
    // indexing success — a transient TEI failure on one file must not wipe
    // its prior index row on the next prune.
    let kept_paths: Vec<String> = walked.iter().map(|w| w.relative_path.clone()).collect();

    let mut report = SyncReport {
        project: project_name,
        project_id,
        dry_run: false,
        files_walked: walked.len(),
        ..SyncReport::default()
    };

    for w in walked {
        process_file(
            w,
            config,
            project_id,
            canonical_repo_str,
            store,
            embedder,
            classifier,
            &mut report,
        )?;
    }

    let pruned = store
        .prune_orphans(project_id, &kept_paths)
        .map_err(SyncError::Store)?;
    report.orphans_pruned = pruned;
    Ok(report)
}

/// Cheap preview: walks, checks the vault.toml cache, computes a cost estimate.
/// Never reads file bodies past what the walker already saw, never embeds, never
/// touches the store, never calls the classifier.
fn dry_run_report(
    walked: &[Walked],
    config: &Config,
    canonical_repo: &Path,
    project_name: String,
) -> SyncReport {
    let canonical_repo_str = canonical_repo.to_str().unwrap_or_default();
    let mut cached = 0usize;
    let mut would_classify = 0usize;
    for w in walked {
        if config
            .cached_classification(canonical_repo_str, &w.relative_path)
            .is_some()
        {
            cached += 1;
        } else {
            would_classify += 1;
        }
    }

    let cost = if config.classifier_mode() == "auto"
        && resolve_backend(config) == ResolvedBackend::Haiku
    {
        cost_estimate(would_classify)
    } else {
        0.0
    };

    SyncReport {
        project: project_name,
        project_id: 0,
        dry_run: true,
        files_walked: walked.len(),
        files_cached: cached,
        files_would_classify: would_classify,
        estimated_haiku_cost_usd: cost,
        ..SyncReport::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn process_file(
    w: &Walked,
    config: &Config,
    project_id: i64,
    canonical_repo_str: &str,
    store: &mut dyn Store,
    embedder: &dyn Embedder,
    classifier: &dyn Classifier,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
    let bytes = match std::fs::read(&w.canonical_path) {
        Ok(b) => b,
        Err(e) => {
            report
                .files_skipped
                .push((w.relative_path.clone(), format!("io error: {e}")));
            return Ok(());
        }
    };
    let content_hash = parse::sha256_hex(&bytes);

    let existing = store
        .get_document_content_hash(project_id, &w.relative_path)
        .map_err(SyncError::Store)?;
    if existing.as_deref() == Some(&content_hash) {
        report.files_unchanged += 1;
        return Ok(());
    }

    let extension = w
        .canonical_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let filename = w
        .canonical_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let classification = match config.cached_classification(canonical_repo_str, &w.relative_path) {
        Some(c) => {
            report.files_cached += 1;
            c
        }
        None => {
            let head_end = bytes.len().min(HEAD_BYTES);
            let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
            if secrets::looks_like_secret(&head) {
                report.files_skipped_remote_classify += 1;
                ext_fallback(&extension)
            } else {
                let input = ClassifyInput {
                    filename: filename.clone(),
                    extension: extension.clone(),
                    head,
                };
                match classifier.classify(&input) {
                    Ok(c) => {
                        report.files_classified += 1;
                        c
                    }
                    Err(e) => {
                        report
                            .files_skipped
                            .push((w.relative_path.clone(), format!("classify error: {e}")));
                        return Ok(());
                    }
                }
            }
        }
    };

    let content_str = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            report
                .files_skipped
                .push((w.relative_path.clone(), "non-utf8 content".to_string()));
            return Ok(());
        }
    };

    let mut chunks = match parser_for(&extension) {
        Some(parser) => match parser.parse(&content_str) {
            Ok(chunks) => {
                report.files_parsed_via_parser += 1;
                chunks
            }
            Err(e) => {
                report
                    .files_skipped
                    .push((w.relative_path.clone(), format!("parse error: {e}")));
                return Ok(());
            }
        },
        None => {
            report.files_parsed_as_whole += 1;
            vec![Chunk {
                language: classification.language,
                label: filename.clone(),
                content: content_str.clone(),
                content_hash: parse::sha256_hex(content_str.as_bytes()),
                token_est: parse::estimate_tokens(&content_str),
                chunk_index: 0,
            }]
        }
    };

    let before = chunks.len();
    chunks.retain(|c| !secrets::looks_like_secret(&c.content));
    report.chunks_dropped_secret += before - chunks.len();

    let mut chunks_with_emb: Vec<ChunkWithEmbedding> = Vec::with_capacity(chunks.len());
    for c in chunks {
        match embedder.embed_document(&c.content) {
            Ok(embedding) => chunks_with_emb.push(ChunkWithEmbedding {
                chunk: c,
                embedding,
            }),
            Err(e) => {
                report
                    .files_skipped
                    .push((w.relative_path.clone(), format!("embed error: {e}")));
                // Bail on this file; the path stays in kept_paths so its prior
                // index entry survives the post-loop prune.
                return Ok(());
            }
        }
    }

    // If every chunk tripped the secret scan there's nothing useful to store —
    // a chunkless document row is harmless (never retrievable) but noise. Record
    // the skip so the report still reflects what happened.
    if chunks_with_emb.is_empty() {
        report.files_skipped.push((
            w.relative_path.clone(),
            "all chunks dropped as secrets".to_string(),
        ));
        return Ok(());
    }

    let doc = Document {
        project_id,
        doc_type: classification.doc_type,
        source_path: w.relative_path.clone(),
        title: filename,
        content_hash,
    };
    let n = chunks_with_emb.len();
    store
        .upsert_document(&doc, &chunks_with_emb)
        .map_err(SyncError::Store)?;
    report.chunks_indexed += n;
    Ok(())
}

fn ext_fallback(extension: &str) -> Classification {
    // Used when the head-guard trips on remote-classify. doc_type defaults to
    // Convention per the plan — head-guard hits are rare and a uniform fallback
    // is good enough until a user pins it via `[classifications.*]`.
    let language = match extension.to_ascii_lowercase().as_str() {
        "proto" => Language::Proto,
        "go" => Language::Go,
        "rs" => Language::Rust,
        "md" => Language::Markdown,
        _ => Language::Unknown,
    };
    Classification {
        doc_type: DocType::Convention,
        language,
    }
}

fn derive_project_name(canonical_repo: &Path) -> String {
    canonical_repo
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("vault-project")
        .to_string()
}

fn prompt_for_haiku_cost<R: BufRead, W: Write>(
    file_count: usize,
    mut stdin: R,
    mut stderr: W,
) -> Result<(), SyncError> {
    let cost = cost_estimate(file_count);
    let _ = writeln!(
        stderr,
        "Gemma not detected. Use Haiku for classification? \
         Estimated cost: ~${cost:.2} for {file_count} files. [y/N] "
    );
    let _ = stderr.flush();
    let mut line = String::new();
    let read = stdin.read_line(&mut line).map_err(SyncError::Io)?;
    // EOF (piped / CI) reads 0 bytes → empty line → treated as "not y". Safe
    // default: a non-interactive run that hasn't been wired to confirm bails
    // rather than silently bills.
    let trimmed = line.trim();
    if read > 0 && (trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")) {
        Ok(())
    } else {
        Err(SyncError::DeclinedHaikuCost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CachedClassification;
    use crate::embed::StubEmbedder;
    use crate::retrieve::QueryPlan;
    use crate::store::{Hit, RetrievalLogEntry};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---------- Test support ----------

    /// One temp dir per test; cleaned up on drop.
    struct Tmp {
        root: PathBuf,
    }
    impl Tmp {
        fn new(label: &str) -> Self {
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!("vault-sync-{label}-{pid}-{nanos}"));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn write(&self, rel: &str, body: &[u8]) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, body).unwrap();
        }
        fn canonical(&self) -> PathBuf {
            fs::canonicalize(&self.root).unwrap()
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Hash-aware stub store. `hashes` is pre-seeded for the unchanged-file
    /// gate; `upserts` / `prunes` are recorded for after-the-fact assertions.
    struct StubStore {
        hashes: HashMap<(i64, String), String>,
        upserts: RefCell<Vec<String>>,
        prunes: RefCell<Vec<Vec<String>>>,
        conflict_on_get_or_create: Option<String>,
    }
    impl StubStore {
        fn new() -> Self {
            Self {
                hashes: HashMap::new(),
                upserts: RefCell::new(Vec::new()),
                prunes: RefCell::new(Vec::new()),
                conflict_on_get_or_create: None,
            }
        }
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
            if let Some(msg) = &self.conflict_on_get_or_create {
                return Err(StoreError::Conflict(msg.clone()));
            }
            Ok(1)
        }
        fn get_document_content_hash(
            &self,
            project_id: i64,
            source_path: &str,
        ) -> Result<Option<String>, StoreError> {
            Ok(self
                .hashes
                .get(&(project_id, source_path.to_string()))
                .cloned())
        }
        fn upsert_document(
            &mut self,
            doc: &Document,
            _chunks: &[ChunkWithEmbedding],
        ) -> Result<(), StoreError> {
            self.upserts.borrow_mut().push(doc.source_path.clone());
            Ok(())
        }
        fn prune_orphans(
            &mut self,
            _project_id: i64,
            kept_paths: &[String],
        ) -> Result<usize, StoreError> {
            self.prunes.borrow_mut().push(kept_paths.to_vec());
            Ok(0)
        }
        // Indexer never retrieves; the two primitives satisfy the trait and the
        // provided hybrid_search is never exercised here.
        fn bm25_search(&self, _plan: &QueryPlan, _top_k: usize) -> Result<Vec<Hit>, StoreError> {
            Ok(vec![])
        }
        fn cosine_search(
            &self,
            _plan: &QueryPlan,
            _embedding: &[f32],
            _top_k: usize,
        ) -> Result<Vec<Hit>, StoreError> {
            Ok(vec![])
        }
        fn log_retrieval(&mut self, _entry: &RetrievalLogEntry) -> Result<(), StoreError> {
            Ok(())
        }
    }

    /// Wraps a classifier, counts calls. Lets us assert classifier-not-called
    /// on the cache + head-guard + unchanged paths.
    struct CountingClassifier<C: Classifier> {
        inner: C,
        calls: AtomicUsize,
    }
    impl<C: Classifier> CountingClassifier<C> {
        fn new(inner: C) -> Self {
            Self {
                inner,
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl<C: Classifier> Classifier for CountingClassifier<C> {
        fn classify(&self, input: &ClassifyInput) -> Result<Classification, ClassifyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.classify(input)
        }
    }

    /// Extension-based classifier (mirrors classify::stub::StubClassifier;
    /// inlined here so this module doesn't reach across cfg(test) boundaries).
    struct ExtClassifier;
    impl Classifier for ExtClassifier {
        fn classify(&self, input: &ClassifyInput) -> Result<Classification, ClassifyError> {
            let (doc_type, language) = match input.extension.to_ascii_lowercase().as_str() {
                "proto" => (DocType::Contract, Language::Proto),
                "go" => (DocType::Convention, Language::Go),
                "rs" => (DocType::Convention, Language::Rust),
                "md" => (DocType::Convention, Language::Markdown),
                _ => (DocType::Convention, Language::Unknown),
            };
            Ok(Classification { doc_type, language })
        }
    }

    /// Embedder that fails on any text body containing `fail_substr`. Mirrors
    /// the "transient TEI failure on one file" scenario in tests 7 + 10.
    struct FailOnTextEmbedder {
        fail_substr: &'static str,
        inner: StubEmbedder,
    }
    impl Embedder for FailOnTextEmbedder {
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed_document(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            if text.contains(self.fail_substr) {
                Err(EmbedError::Transport("simulated".to_string()))
            } else {
                self.inner.embed_document(text)
            }
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.inner.embed_query(text)
        }
    }

    fn opts(repo: &Path, dry: bool) -> SyncOptions {
        SyncOptions {
            repo: repo.to_path_buf(),
            explicit_name: Some("test-project".to_string()),
            dry_run: dry,
        }
    }

    fn config_with_cache(repo: &Path, pattern: &str, cls: CachedClassification) -> Config {
        // Round-trip via TOML so we don't poke private fields. The cache
        // normalizes keys, so the canonical tempdir path works directly.
        let key = repo.to_str().unwrap();
        let doc_type = cls.doc_type.as_str();
        let language = cls.language.as_str();
        let toml_text = format!(
            r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15
timeout = 3

[router]
mode = "auto"
model = "haiku"

[mlx]
endpoint = "http://localhost:8080"
router_model = "test"

[embeddings]
endpoint = "http://localhost:8081"
model = "nomic-ai/nomic-embed-text-v1.5"
dims = 768

[classifications."{key}"]
"{pattern}" = {{ doc_type = "{doc_type}", language = "{language}" }}
"#
        );
        toml::from_str(&toml_text).expect("parse")
    }

    // ---------- Plan's 10 cases ----------

    // 1. Happy path: tempdir + .proto + .go, stubs, classifier called twice.
    #[test]
    fn happy_path_classifies_parses_embeds() {
        let tmp = Tmp::new("happy");
        tmp.write("a.proto", b"syntax = \"proto3\";\nmessage A {}");
        tmp.write("nested/b.go", b"package main\nfunc Foo() {}");
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = CountingClassifier::new(ExtClassifier);

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "test-project".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .expect("ok");

        assert_eq!(report.files_walked, 2);
        assert_eq!(report.files_classified, 2);
        assert_eq!(classifier.calls(), 2);
        assert!(report.chunks_indexed > 0);
        assert_eq!(report.orphans_pruned, 0);
        assert_eq!(store.upserts.borrow().len(), 2);
    }

    // 2. Unchanged gate: pre-seed real sha256 → classifier never called.
    #[test]
    fn unchanged_gate_short_circuits_classifier_and_upsert() {
        let tmp = Tmp::new("unchanged");
        let body = b"package main\nfunc Foo() {}";
        tmp.write("a.go", body);
        let canonical = tmp.canonical();
        let real_hash = parse::sha256_hex(body);

        let mut store = StubStore::new();
        store.hashes.insert((1, "a.go".to_string()), real_hash);

        let config = Config::default();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = CountingClassifier::new(ExtClassifier);

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert_eq!(report.files_unchanged, 1);
        assert_eq!(classifier.calls(), 0, "unchanged file must not classify");
        assert!(
            store.upserts.borrow().is_empty(),
            "unchanged file must not upsert"
        );
    }

    // 3. Cache override hit: classifier zero calls; files_cached == 1.
    #[test]
    fn cache_override_skips_classifier() {
        let tmp = Tmp::new("cache");
        tmp.write("svc.proto", b"syntax = \"proto3\";\nmessage S {}");
        let canonical = tmp.canonical();

        let cached = CachedClassification {
            doc_type: DocType::Contract,
            language: Language::Proto,
        };
        let config = config_with_cache(&canonical, "**/*.proto", cached);
        let mut store = StubStore::new();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = CountingClassifier::new(ExtClassifier);

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert_eq!(report.files_cached, 1);
        assert_eq!(classifier.calls(), 0);
    }

    // 4. Head-guard bypass: PEM header in first 1 KiB triggers ext fallback.
    #[test]
    fn head_guard_bypasses_remote_classifier() {
        let tmp = Tmp::new("head-guard");
        tmp.write(
            "leaked.txt",
            b"-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA",
        );
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = CountingClassifier::new(ExtClassifier);

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert_eq!(report.files_skipped_remote_classify, 1);
        assert_eq!(classifier.calls(), 0, "head-guard hit must not classify");
    }

    // 5. Chunk secret drop: file body contains an AWS key.
    #[test]
    fn chunk_secret_scan_drops_matching_chunks() {
        let tmp = Tmp::new("secret-chunk");
        // .txt has no parser → whole-file chunk containing the AWS key.
        tmp.write(
            "leak.txt",
            b"// nothing to see\nAKIA0123456789ABCDEF\nmore text",
        );
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = ExtClassifier;

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert!(
            report.chunks_dropped_secret >= 1,
            "expected at least one dropped chunk"
        );
        // Single-chunk file + only chunk dropped → no upsert and the file is
        // recorded in files_skipped instead of leaving a chunkless doc row.
        assert!(store.upserts.borrow().is_empty());
        assert_eq!(report.files_skipped.len(), 1);
    }

    // 6. Dry-run: stubs / classifier / embedder never touched; correct counters.
    #[test]
    fn dry_run_skips_classifier_embedder_and_store() {
        let tmp = Tmp::new("dry");
        tmp.write("a.proto", b"syntax = \"proto3\";");
        tmp.write("b.go", b"package main");
        let canonical = tmp.canonical();

        let config = Config::default();
        let report = run_sync(opts(&canonical, true), &config).expect("dry-run ok");

        assert!(report.dry_run);
        assert_eq!(report.files_walked, 2);
        assert_eq!(report.files_would_classify, 2);
        assert_eq!(report.files_classified, 0);
        assert_eq!(report.chunks_indexed, 0);
        assert_eq!(report.orphans_pruned, 0);
        assert_eq!(report.project_id, 0);
    }

    // 7. Prune sees the full walked set even when one file errors on embed.
    //    Uses .txt files (no parser → whole-file chunk) so the failing
    //    substring reliably reaches the embedder body.
    #[test]
    fn prune_keeps_paths_for_files_that_failed_to_embed() {
        let tmp = Tmp::new("prune-keep");
        tmp.write("ok.txt", b"healthy content");
        tmp.write("flaky.txt", b"content with fail-me marker");
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        let embedder = FailOnTextEmbedder {
            fail_substr: "fail-me",
            inner: StubEmbedder::from_config(&config),
        };
        let classifier = ExtClassifier;

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert_eq!(report.files_skipped.len(), 1, "flaky.txt embed failed");
        let prunes = store.prunes.borrow();
        assert_eq!(prunes.len(), 1, "exactly one prune call expected");
        let kept = &prunes[0];
        assert!(kept.contains(&"ok.txt".to_string()));
        assert!(
            kept.contains(&"flaky.txt".to_string()),
            "flaky.txt must stay in kept_paths so its prior index entry survives prune"
        );
    }

    // 8. No-parser fallback: .txt → one whole-file chunk.
    #[test]
    fn no_parser_fallback_emits_whole_file_chunk() {
        let tmp = Tmp::new("nofallback");
        tmp.write("notes.txt", b"just some text\nnot a recognized language");
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = ExtClassifier;

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert_eq!(report.files_parsed_as_whole, 1);
        assert_eq!(report.files_parsed_via_parser, 0);
        assert_eq!(report.chunks_indexed, 1);
    }

    // 9. Project name collision mapping: Store::Conflict → SyncError::ProjectNameCollision.
    #[test]
    fn project_name_collision_maps_to_sync_error() {
        let mut store = StubStore::new();
        store.conflict_on_get_or_create =
            Some("name 'p' already at /a; this targets /b".to_string());

        let result = store.get_or_create_project("p", "/b").map_err(|e| match e {
            StoreError::Conflict(msg) => SyncError::ProjectNameCollision {
                name: "p".to_string(),
                message: msg,
            },
            other => SyncError::Store(other),
        });
        match result {
            Err(SyncError::ProjectNameCollision { name, message }) => {
                assert_eq!(name, "p");
                assert!(message.contains("/a"));
            }
            other => panic!("expected ProjectNameCollision, got {other:?}"),
        }
    }

    // 10. Transient embed failure does NOT drop the file from kept_paths,
    //     even when the store has a prior content_hash for it. Uses a .txt
    //     file (no parser → whole-file chunk) so the fail substring is
    //     guaranteed to be in the embedded chunk content.
    #[test]
    fn transient_embed_failure_does_not_prune_prior_index() {
        let tmp = Tmp::new("transient");
        tmp.write("flaky.txt", b"some content with fail-me marker");
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        // Prior index entry with a different hash → unchanged-gate doesn't
        // short-circuit, so the flow proceeds through embed (which fails).
        store
            .hashes
            .insert((1, "flaky.txt".to_string()), "stale-hash".to_string());

        let embedder = FailOnTextEmbedder {
            fail_substr: "fail-me",
            inner: StubEmbedder::from_config(&config),
        };
        let classifier = ExtClassifier;

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &canonical,
            &config,
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert_eq!(report.files_skipped.len(), 1, "embed failure recorded");
        let prunes = store.prunes.borrow();
        assert_eq!(prunes.len(), 1);
        assert!(
            prunes[0].contains(&"flaky.txt".to_string()),
            "flaky.txt must be in kept_paths so prune doesn't delete its prior row"
        );
    }

    // ---------- Prompt unit tests ----------

    #[test]
    fn prompt_returns_ok_on_y() {
        let input = b"y\n".to_vec();
        let mut err = Vec::new();
        prompt_for_haiku_cost(10, &input[..], &mut err).expect("y accepted");
    }

    #[test]
    fn prompt_declines_on_empty_stdin_eof() {
        let input: &[u8] = b"";
        let mut err = Vec::new();
        let r = prompt_for_haiku_cost(10, input, &mut err);
        assert!(matches!(r, Err(SyncError::DeclinedHaikuCost)));
    }

    #[test]
    fn prompt_declines_on_n() {
        let input = b"n\n".to_vec();
        let mut err = Vec::new();
        let r = prompt_for_haiku_cost(10, &input[..], &mut err);
        assert!(matches!(r, Err(SyncError::DeclinedHaikuCost)));
    }
}
