use std::collections::BTreeMap;
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
use crate::parse::{self, select_parser};
use crate::store::{Chunk, ChunkWithEmbedding, Document, SqliteStore, Store, StoreError};
use crate::types::{DocType, Language};

const HEAD_BYTES: usize = 1024;

pub struct SyncOptions {
    pub repo: PathBuf,
    pub explicit_name: Option<String>,
    pub explicit_domain: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub project: String,
    pub project_id: i64,        // 0 in dry-run (no store touched)
    pub domain: Option<String>, // project's domain assignment (None = unassigned; always None in dry-run)
    pub dry_run: bool,
    pub files_walked: usize,
    pub files_classified: usize, // called the live classifier (0 in dry-run)
    pub files_would_classify: usize, // dry-run only — # that would have classified
    pub files_unchanged: usize,  // content_hash matched (0 in dry-run)
    pub files_skipped_remote_classify: usize, // head trip → ext fallback (0 in dry-run)
    pub files_parsed_via_parser: usize, // 0 in dry-run
    pub files_parsed_as_whole: usize, // 0 in dry-run
    // Label distribution: "doc_type/language" → count, over every file the
    // classifier (or ext fallback) labeled this run. Surfaces a systematic
    // misclassification (e.g. protos landing as plan/whole-file) — the
    // observability that replaces the dropped confirm/override UX.
    pub classifications: BTreeMap<String, usize>,
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
    #[error("declined remote classification cost — sync aborted")]
    DeclinedRemoteCost,
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
    let derived_name = derive_project_name(&canonical);

    let walked = walk_repo(
        &canonical,
        &WalkOptions {
            user_extra_excludes: config.indexer_exclude_patterns().to_vec(),
        },
    )
    .map_err(SyncError::Walk)?;

    if opts.dry_run {
        // Dry-run is a non-interactive preview — never prompt; use the explicit
        // or derived name silently.
        let project_name = opts.explicit_name.clone().unwrap_or(derived_name);
        return Ok(dry_run_report(&walked, config, project_name));
    }

    // First-run name confirmation: with no `--name`, offer the directory-derived
    // default for the user to accept (empty line / EOF) or override. The chosen
    // name is persisted by `get_or_create_project` below; vault.toml is never
    // written.
    let project_name = match opts.explicit_name.clone() {
        Some(name) => name,
        None => prompt_for_project_name(&derived_name, std::io::stdin().lock(), std::io::stderr())?,
    };

    let embedder = TeiEmbedder::from_config(config).map_err(SyncError::TeiUnreachable)?;
    embedder
        .verify_against_server()
        .map_err(SyncError::TeiUnreachable)?;

    let mode = config.classifier_mode();
    let backend = resolve_backend(config);
    if mode == "auto" && !walked.is_empty() {
        match backend {
            ResolvedBackend::Haiku => {
                prompt_for_haiku_cost(walked.len(), std::io::stdin().lock(), std::io::stderr())?;
            }
            // The OpenAI-compatible backend bills per provider; we don't carry a
            // pricing table, so confirm generically rather than quote a figure.
            ResolvedBackend::OpenAiCompat => {
                prompt_for_remote_cost(walked.len(), std::io::stdin().lock(), std::io::stderr())?;
            }
            ResolvedBackend::Gemma => {}
        }
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

    // First-run domain assignment. Skip silently if the project already has one
    // (re-sync). Otherwise take `--domain`, else prompt; empty / EOF /
    // non-interactive stdin leaves it unassigned (the hook then falls back to
    // defaults.context_tag). Assignment lives in vault.db; the context tag is
    // derived by convention as `{domain}-context`, never stored.
    let domain = match store
        .resolve_domain(std::slice::from_ref(&project_name))
        .map_err(SyncError::Store)?
    {
        Some(existing) => Some(existing),
        None => {
            let chosen = match opts.explicit_domain.clone() {
                Some(d) => Some(d),
                None => prompt_for_domain(std::io::stdin().lock(), std::io::stderr())?,
            };
            if let Some(ref d) = chosen {
                store
                    .set_project_domain(project_id, d)
                    .map_err(SyncError::Store)?;
                // A new domain needs matching framing in the user's global
                // CLAUDE.md or the emitted tag means nothing to Claude — the
                // taxonomy's single source of truth (see `docs/vault-plan.md`).
                let _ = writeln!(
                    std::io::stderr(),
                    "Assigned to domain {d:?} (context tag <{d}-context>). \
                     Add a `## {d}-context` section to ~/.claude/CLAUDE.md so Claude \
                     interprets the tag."
                );
            }
            chosen
        }
    };

    let mut report = sync_with(
        project_id,
        project_name,
        &walked,
        &mut store,
        &embedder,
        classifier.as_ref(),
    )?;
    report.domain = domain;
    Ok(report)
}

/// Core orchestrator. Takes trait objects so tests can swap stubs without
/// constructing TEI / SQLite. Caller is responsible for the cost-prompt and
/// project-id resolution.
pub(crate) fn sync_with(
    project_id: i64,
    project_name: String,
    walked: &[Walked],
    store: &mut dyn Store,
    embedder: &dyn Embedder,
    classifier: &dyn Classifier,
) -> Result<SyncReport, SyncError> {
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
        process_file(w, project_id, store, embedder, classifier, &mut report)?;
    }

    let pruned = store
        .prune_orphans(project_id, &kept_paths)
        .map_err(SyncError::Store)?;
    report.orphans_pruned = pruned;
    Ok(report)
}

/// Cheap preview: walks and computes a cost estimate. Never reads file bodies
/// past what the walker already saw, never embeds, never touches the store,
/// never calls the classifier. Because it doesn't open the DB it can't tell
/// which files are unchanged, so the estimate is an upper bound over every
/// walked file.
fn dry_run_report(walked: &[Walked], config: &Config, project_name: String) -> SyncReport {
    let would_classify = walked.len();
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
        files_would_classify: would_classify,
        estimated_haiku_cost_usd: cost,
        ..SyncReport::default()
    }
}

fn process_file(
    w: &Walked,
    project_id: i64,
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

    let classification = {
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
    };

    *report
        .classifications
        .entry(format!(
            "{}/{}",
            classification.doc_type.as_str(),
            classification.language.as_str()
        ))
        .or_default() += 1;

    let content_str = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            report
                .files_skipped
                .push((w.relative_path.clone(), "non-utf8 content".to_string()));
            return Ok(());
        }
    };

    let mut chunks =
        match select_parser(classification.doc_type, classification.language, &extension) {
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

    // Nothing to store. Two distinct causes, reported distinctly so the user can
    // tell them apart: either the parser produced no chunks at all (the file
    // isn't being parsed into anything indexable — e.g. a binary entrypoint with
    // no exported symbols, or a re-export module), or it produced chunks that
    // were all dropped by the secret scan. Conflating these as "secrets" hides
    // files that silently contribute nothing to the index.
    if chunks_with_emb.is_empty() {
        let reason = if before == 0 {
            "produced no chunks — nothing indexable parsed".to_string()
        } else {
            "all chunks dropped as secrets".to_string()
        };
        report.files_skipped.push((w.relative_path.clone(), reason));
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
    // is good enough for them.
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

/// Human-readable rendering of a `SyncReport` for `vault index sync`. Branches
/// on `report.dry_run` because counters that are intentionally zero in dry-run
/// (parse/embed/upsert/prune) would mislead the reader if printed unconditionally.
pub fn format_report(report: &SyncReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if report.dry_run {
        let _ = writeln!(
            out,
            "Dry run for project {:?} (no DB or remote services touched)",
            report.project
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  Walked:                 {} files",
            report.files_walked
        );
        let _ = writeln!(
            out,
            "  Would classify:         {}",
            report.files_would_classify
        );
        if report.estimated_haiku_cost_usd > 0.0 {
            let _ = writeln!(
                out,
                "  Estimated Haiku cost:   ${:.4}",
                report.estimated_haiku_cost_usd
            );
        } else {
            let _ = writeln!(
                out,
                "  Estimated Haiku cost:   $0.00 (auto resolved to Gemma)"
            );
        }
    } else {
        let _ = writeln!(
            out,
            "Synced project {:?} (id {})",
            report.project, report.project_id
        );
        let _ = writeln!(out);
        match &report.domain {
            Some(d) => {
                let _ = writeln!(out, "  Domain:                 {d} (tag <{d}-context>)");
            }
            None => {
                let _ = writeln!(
                    out,
                    "  Domain:                 unassigned (uses defaults.context_tag)"
                );
            }
        }
        let _ = writeln!(
            out,
            "  Walked:                 {} files",
            report.files_walked
        );
        let _ = writeln!(out, "  Classified:             {}", report.files_classified);
        let _ = writeln!(
            out,
            "  Skipped remote (head):  {} (secret pre-scan → ext fallback)",
            report.files_skipped_remote_classify
        );
        let _ = writeln!(
            out,
            "  Unchanged:              {} (content_hash matched)",
            report.files_unchanged
        );
        let _ = writeln!(
            out,
            "  Parsed via parser:      {}",
            report.files_parsed_via_parser
        );
        let _ = writeln!(
            out,
            "  Parsed as whole-file:   {}",
            report.files_parsed_as_whole
        );
        if !report.classifications.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "  Label breakdown (doc_type/language):");
            let width = report
                .classifications
                .keys()
                .map(String::len)
                .max()
                .unwrap_or(0);
            for (label, count) in &report.classifications {
                let _ = writeln!(out, "    {label:<width$}  {count}");
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "  Chunks indexed:         {}", report.chunks_indexed);
        let _ = writeln!(
            out,
            "  Chunks dropped (secret): {}",
            report.chunks_dropped_secret
        );
        let _ = writeln!(out, "  Orphans pruned:         {}", report.orphans_pruned);
        if !report.files_skipped.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "  Skipped ({}):", report.files_skipped.len());
            for (path, reason) in &report.files_skipped {
                let _ = writeln!(out, "    - {path}: {reason}");
            }
        }
    }
    out
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
        Err(SyncError::DeclinedRemoteCost)
    }
}

/// Confirmation for the OpenAI-compatible remote classifier on auto→remote
/// fallback. No dollar figure (provider pricing varies and isn't tabled here);
/// the user confirms that paid remote calls are acceptable. Same fail-closed
/// EOF semantics as `prompt_for_haiku_cost`.
fn prompt_for_remote_cost<R: BufRead, W: Write>(
    file_count: usize,
    mut stdin: R,
    mut stderr: W,
) -> Result<(), SyncError> {
    let _ = writeln!(
        stderr,
        "Gemma not detected. Use the configured remote API (openai) for classification? \
         {file_count} files — provider billing applies. [y/N] "
    );
    let _ = stderr.flush();
    let mut line = String::new();
    let read = stdin.read_line(&mut line).map_err(SyncError::Io)?;
    let trimmed = line.trim();
    if read > 0 && (trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")) {
        Ok(())
    } else {
        Err(SyncError::DeclinedRemoteCost)
    }
}

/// Prompt for the project name, defaulting to the directory-derived name on an
/// empty line or EOF (piped / CI). Mirrors `prompt_for_haiku_cost`'s injected
/// reader/writer so it tests without a real terminal. Only called when `--name`
/// was not passed; the chosen name is persisted by `get_or_create_project`.
fn prompt_for_project_name<R: BufRead, W: Write>(
    derived: &str,
    mut stdin: R,
    mut stderr: W,
) -> Result<String, SyncError> {
    let _ = writeln!(stderr, "Project name? [{derived}] ");
    let _ = stderr.flush();
    let mut line = String::new();
    let read = stdin.read_line(&mut line).map_err(SyncError::Io)?;
    let trimmed = line.trim();
    if read > 0 && !trimmed.is_empty() {
        Ok(trimmed.to_string())
    } else {
        Ok(derived.to_string())
    }
}

/// First-run domain prompt. Returns the chosen domain (trimmed, non-empty) or
/// `None` to leave the project unassigned. Empty line / EOF / non-interactive
/// stdin → `None`, mirroring the fail-open contract of the other sync prompts.
/// Only called when `--domain` was not passed and the project has no assignment
/// yet; the choice persists via `Store::set_project_domain`.
fn prompt_for_domain<R: BufRead, W: Write>(
    mut stdin: R,
    mut stderr: W,
) -> Result<Option<String>, SyncError> {
    let _ = writeln!(
        stderr,
        "Domain for this project? (e.g. software, finance, personal) [skip] "
    );
    let _ = stderr.flush();
    let mut line = String::new();
    let read = stdin.read_line(&mut line).map_err(SyncError::Io)?;
    let trimmed = line.trim();
    if read > 0 && !trimmed.is_empty() {
        Ok(Some(trimmed.to_string()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            explicit_domain: None,
            dry_run: dry,
        }
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
        // Label breakdown tallies each assigned (doc_type/language).
        assert_eq!(report.classifications.get("contract/proto"), Some(&1));
        assert_eq!(report.classifications.get("convention/go"), Some(&1));
        assert_eq!(report.classifications.values().sum::<usize>(), 2);
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

    // 3. Head-guard bypass: PEM header in first 1 KiB triggers ext fallback.
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
        assert_eq!(
            report.files_skipped[0].1, "all chunks dropped as secrets",
            "a real secret drop keeps the secret reason"
        );
    }

    // 5b. Parser produces zero chunks: reported as unparsed, not as a secret drop,
    // so the user can spot files that silently contribute nothing to the index.
    #[test]
    fn file_with_no_indexable_chunks_is_reported_as_unparsed() {
        let tmp = Tmp::new("no-chunks");
        // .rs with no exported (pub) symbols → rust parser yields zero chunks.
        tmp.write("empty.rs", b"// only a comment, no exported symbols\n");
        let canonical = tmp.canonical();

        let config = Config::default();
        let mut store = StubStore::new();
        let embedder = StubEmbedder::from_config(&config);
        let classifier = ExtClassifier;

        let walked = walk_repo(&canonical, &WalkOptions::default()).unwrap();
        let report = sync_with(
            1,
            "p".to_string(),
            &walked,
            &mut store,
            &embedder,
            &classifier,
        )
        .unwrap();

        assert!(store.upserts.borrow().is_empty());
        assert_eq!(
            report.chunks_dropped_secret, 0,
            "no secret was involved — must not be attributed to the secret scan"
        );
        assert_eq!(report.files_skipped.len(), 1);
        assert_eq!(report.files_skipped[0].0, "empty.rs");
        assert_eq!(
            report.files_skipped[0].1,
            "produced no chunks — nothing indexable parsed"
        );
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
        assert!(matches!(r, Err(SyncError::DeclinedRemoteCost)));
    }

    #[test]
    fn prompt_declines_on_n() {
        let input = b"n\n".to_vec();
        let mut err = Vec::new();
        let r = prompt_for_haiku_cost(10, &input[..], &mut err);
        assert!(matches!(r, Err(SyncError::DeclinedRemoteCost)));
    }

    #[test]
    fn remote_cost_prompt_accepts_y_and_declines_eof() {
        let mut err = Vec::new();
        prompt_for_remote_cost(5, &b"y\n"[..], &mut err).expect("y accepted");

        let empty: &[u8] = b"";
        let mut err2 = Vec::new();
        let r = prompt_for_remote_cost(5, empty, &mut err2);
        assert!(matches!(r, Err(SyncError::DeclinedRemoteCost)));
    }

    #[test]
    fn project_name_prompt_uses_input_when_provided() {
        let input = b"my-service\n".to_vec();
        let mut err = Vec::new();
        let name = prompt_for_project_name("derived", &input[..], &mut err).expect("name");
        assert_eq!(name, "my-service");
        // The derived default is surfaced in the prompt text.
        assert!(String::from_utf8_lossy(&err).contains("[derived]"));
    }

    #[test]
    fn project_name_prompt_defaults_on_empty_line() {
        let input = b"\n".to_vec();
        let mut err = Vec::new();
        let name = prompt_for_project_name("derived", &input[..], &mut err).expect("name");
        assert_eq!(name, "derived");
    }

    #[test]
    fn project_name_prompt_defaults_on_eof() {
        let input: &[u8] = b"";
        let mut err = Vec::new();
        let name = prompt_for_project_name("derived", input, &mut err).expect("name");
        assert_eq!(name, "derived");
    }

    #[test]
    fn project_name_prompt_trims_surrounding_whitespace() {
        let input = b"  spaced-name  \n".to_vec();
        let mut err = Vec::new();
        let name = prompt_for_project_name("derived", &input[..], &mut err).expect("name");
        assert_eq!(name, "spaced-name");
    }

    #[test]
    fn domain_prompt_uses_input_when_provided() {
        let input = b"finance\n".to_vec();
        let mut err = Vec::new();
        let domain = prompt_for_domain(&input[..], &mut err).expect("domain");
        assert_eq!(domain, Some("finance".to_string()));
    }

    #[test]
    fn domain_prompt_skips_on_empty_line() {
        let input = b"\n".to_vec();
        let mut err = Vec::new();
        let domain = prompt_for_domain(&input[..], &mut err).expect("domain");
        assert_eq!(domain, None);
    }

    #[test]
    fn domain_prompt_skips_on_eof() {
        let input: &[u8] = b"";
        let mut err = Vec::new();
        let domain = prompt_for_domain(input, &mut err).expect("domain");
        assert_eq!(domain, None);
    }

    #[test]
    fn domain_prompt_trims_surrounding_whitespace() {
        let input = b"  software  \n".to_vec();
        let mut err = Vec::new();
        let domain = prompt_for_domain(&input[..], &mut err).expect("domain");
        assert_eq!(domain, Some("software".to_string()));
    }

    // ---------- format_report ----------

    #[test]
    fn format_dry_run_with_gemma_shows_zero_cost_note() {
        let r = SyncReport {
            project: "vault".into(),
            dry_run: true,
            files_walked: 60,
            files_would_classify: 60,
            estimated_haiku_cost_usd: 0.0,
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(s.contains("Dry run for project \"vault\""));
        assert!(s.contains("Walked:                 60 files"));
        assert!(s.contains("Would classify:         60"));
        assert!(s.contains("auto resolved to Gemma"));
        assert!(!s.contains("Chunks indexed"));
    }

    #[test]
    fn format_dry_run_with_haiku_shows_dollar_estimate() {
        let r = SyncReport {
            project: "vault".into(),
            dry_run: true,
            files_walked: 60,
            files_would_classify: 60,
            estimated_haiku_cost_usd: 0.012,
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(s.contains("Estimated Haiku cost:   $0.0120"));
    }

    #[test]
    fn format_real_sync_lists_all_counters() {
        let r = SyncReport {
            project: "vault".into(),
            project_id: 7,
            dry_run: false,
            files_walked: 60,
            files_classified: 50,
            files_skipped_remote_classify: 1,
            files_unchanged: 4,
            files_parsed_via_parser: 45,
            files_parsed_as_whole: 10,
            chunks_indexed: 342,
            chunks_dropped_secret: 4,
            orphans_pruned: 2,
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(s.contains("Synced project \"vault\" (id 7)"));
        assert!(s.contains("Classified:             50"));
        assert!(s.contains("Chunks indexed:         342"));
        assert!(s.contains("Orphans pruned:         2"));
        // Dry-run-only counters absent from real-sync output
        assert!(!s.contains("Estimated Haiku cost"));
        assert!(!s.contains("Would classify"));
    }

    #[test]
    fn format_real_sync_renders_label_breakdown() {
        let mut classifications = BTreeMap::new();
        classifications.insert("contract/proto".to_string(), 5);
        classifications.insert("convention/rust".to_string(), 12);
        let r = SyncReport {
            project: "vault".into(),
            dry_run: false,
            classifications,
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(s.contains("Label breakdown (doc_type/language):"));
        assert!(s.contains("contract/proto"));
        // Widest key (convention/rust) needs no padding, so the count abuts it.
        assert!(s.contains("convention/rust  12"));
    }

    #[test]
    fn format_real_sync_omits_label_breakdown_when_empty() {
        let r = SyncReport {
            project: "vault".into(),
            dry_run: false,
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(!s.contains("Label breakdown"));
    }

    #[test]
    fn format_real_sync_lists_skipped_files_when_present() {
        let r = SyncReport {
            project: "vault".into(),
            dry_run: false,
            files_walked: 2,
            files_classified: 1,
            files_skipped: vec![("src/foo.tmp".into(), "io error".into())],
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(s.contains("Skipped (1):"));
        assert!(s.contains("    - src/foo.tmp: io error"));
    }

    #[test]
    fn format_real_sync_omits_skipped_section_when_empty() {
        let r = SyncReport {
            project: "vault".into(),
            dry_run: false,
            ..SyncReport::default()
        };
        let s = format_report(&r);
        assert!(!s.contains("Skipped ("));
    }
}
