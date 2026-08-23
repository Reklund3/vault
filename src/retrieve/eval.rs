//! Golden-prompt evaluation — the ground truth review finding C2 asked for.
//!
//! Everything tuned before this was measured on a single hand-run prompt, which
//! is how `alpha` got picked and how three separate indexing changes each
//! "looked like an improvement" with no way to see their combined effect. A
//! fixture set turns that into a number.
//!
//! Shape of the harness, and why:
//!
//! - **Real source files, real embeddings.** The corpus is this repo, parsed by
//!   the production parsers and embedded by TEI. A synthetic corpus would
//!   measure the harness rather than the retriever, and a stub embedder makes
//!   cosine meaningless — which is the arm most of the tuning is about.
//! - **No classifier.** `doc_type`/`language` come from the extension rule
//!   below, so a run costs no API calls and cannot drift with a model. What is
//!   under test is retrieval, not labelling.
//! - **Assertions on labels, never content.** Labels survive ordinary edits.
//!   A fixture that breaks on every refactor gets deleted rather than fixed.
//! - **Gated on live TEI**, matching the convention in `embed/tei.rs`.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::embed::{Embedder, TeiEmbedder};
use crate::parse::{select_parser, whole_file_chunks};
use crate::retrieve::QueryPlan;
use crate::store::{ChunkWithEmbedding, Document, SqliteStore, Store};
use crate::types::{DocType, Language};

const FIXTURES: &str = include_str!("golden.toml");

#[derive(serde::Deserialize)]
struct Fixtures {
    corpus: Vec<String>,
    #[serde(rename = "case")]
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    prompt: String,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    type_names: Vec<String>,
    expect: Vec<String>,
}

impl Case {
    fn plan(&self) -> QueryPlan {
        QueryPlan {
            projects: vec!["vault".into()],
            type_names: self.type_names.clone(),
            topics: self.topics.clone(),
            doc_types: vec![],
            languages: vec![],
        }
    }
}

/// Deterministic stand-in for the classifier. Mirrors how this repo is actually
/// labelled — `src/**.rs` is convention, `docs/**.md` is plan, top-level
/// markdown is meta — without a network call or a model in the loop.
fn label_for(path: &str) -> (DocType, Language) {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let language = match ext {
        "rs" => Language::Rust,
        "go" => Language::Go,
        "proto" => Language::Proto,
        "md" => Language::Markdown,
        _ => Language::Unknown,
    };
    let doc_type = match (language, path.starts_with("docs/")) {
        (Language::Markdown, true) => DocType::Plan,
        (Language::Markdown, false) => DocType::Meta,
        (Language::Proto, _) => DocType::Contract,
        _ => DocType::Convention,
    };
    (doc_type, language)
}

/// Index the fixture corpus into a fresh in-memory store.
fn build_corpus(fx: &Fixtures, config: &Config, embedder: &dyn Embedder) -> SqliteStore {
    let mut store = SqliteStore::open_in_memory(config).expect("store");
    let project_id = store
        .get_or_create_project("vault", "/eval")
        .expect("project");

    for path in &fx.corpus {
        let content = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let (doc_type, language) = label_for(path);
        let filename = Path::new(path)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(path);
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        // Same dispatch order as `index::sync`, including the empty-parse
        // fallback, so the eval corpus is chunked exactly as a real sync would.
        let parsed = select_parser(doc_type, language, ext).and_then(|p| p.parse(&content).ok());
        let chunks = match parsed {
            Some(c) if !c.is_empty() => c,
            _ => whole_file_chunks(&content, language, filename).0,
        };

        let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
        let embeddings = embedder.embed_documents(&texts).expect("embed corpus");
        let with_emb: Vec<ChunkWithEmbedding> = chunks
            .into_iter()
            .zip(embeddings)
            .map(|(chunk, embedding)| ChunkWithEmbedding { chunk, embedding })
            .collect();

        store
            .upsert_document(
                &Document {
                    project_id,
                    doc_type,
                    source_path: path.clone(),
                    title: filename.to_string(),
                    content_hash: format!("eval-{path}"),
                },
                &with_emb,
            )
            .expect("upsert");
    }
    store
}

/// What one case scored. `first_rank` is the headline: burying the answer at #18
/// is a different failure from not retrieving it at all, and a recall figure
/// alone cannot tell them apart.
struct Score {
    name: String,
    first_rank: Option<usize>,
    found: usize,
    expected: usize,
    missing: Vec<String>,
}

fn run_case(case: &Case, store: &SqliteStore, embedder: &dyn Embedder, alpha: f32) -> Score {
    let embedding = embedder.embed_query(&case.prompt).expect("embed query");
    let hits = store
        .hybrid_search(&case.plan(), &embedding, alpha)
        .expect("search");
    let labels: Vec<&str> = hits.iter().map(|h| h.label.as_str()).collect();

    let first_rank = case
        .expect
        .iter()
        .filter_map(|e| labels.iter().position(|l| l == e).map(|p| p + 1))
        .min();
    let missing: Vec<String> = case
        .expect
        .iter()
        .filter(|e| !labels.contains(&e.as_str()))
        .cloned()
        .collect();

    Score {
        name: case.name.clone(),
        first_rank,
        found: case.expect.len() - missing.len(),
        expected: case.expect.len(),
        missing,
    }
}

fn load() -> Fixtures {
    toml::from_str(FIXTURES).expect("golden.toml parses")
}

/// The TEI client's 3s default is sized for the hook's single query embed.
/// Building the corpus sends batches of whole parsed files, which blows past it
/// — the same reason `index::sync` carries `SYNC_EMBED_TIMEOUT`.
const EVAL_EMBED_TIMEOUT: Duration = Duration::from_secs(120);

fn harness() -> (Fixtures, Config, TeiEmbedder, SqliteStore) {
    let fx = load();
    let config = Config::default();
    let embedder =
        TeiEmbedder::from_config_with_timeout(&config, EVAL_EMBED_TIMEOUT).expect("TEI client");
    embedder.verify_against_server().expect("TEI reachable");
    let store = build_corpus(&fx, &config, &embedder);
    (fx, config, embedder, store)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture file is data, and data rots silently. This runs without TEI
    /// so a malformed fixture or a corpus path deleted by a refactor fails in
    /// CI rather than the next time someone remembers to run the eval.
    #[test]
    fn fixtures_are_well_formed_and_the_corpus_exists() {
        let fx = load();
        assert!(!fx.cases.is_empty(), "no cases");
        for path in &fx.corpus {
            assert!(
                Path::new(path).exists(),
                "corpus file has moved or been deleted: {path}"
            );
        }
        for case in &fx.cases {
            assert!(!case.expect.is_empty(), "{}: expects nothing", case.name);
            assert!(
                !case.topics.is_empty() || !case.type_names.is_empty(),
                "{}: no keyword signal, the BM25 arm cannot run",
                case.name
            );
        }
    }

    /// The gate. Every expected chunk must be retrieved, and the best one must
    /// land in the top 10 — the region a 10k token budget actually injects.
    #[test]
    #[ignore = "requires live TEI at http://localhost:8081"]
    fn golden_prompts_retrieve_their_expected_chunks() {
        let (fx, config, embedder, store) = harness();
        let alpha = config.alpha();

        let scores: Vec<Score> = fx
            .cases
            .iter()
            .map(|c| run_case(c, &store, &embedder, alpha))
            .collect();

        println!("\nalpha = {alpha}");
        for s in &scores {
            println!(
                "  {:<32} recall {}/{}  first@{}",
                s.name,
                s.found,
                s.expected,
                s.first_rank
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "MISS".into())
            );
        }

        let failures: Vec<&Score> = scores
            .iter()
            .filter(|s| !s.missing.is_empty() || s.first_rank.is_none_or(|r| r > 10))
            .collect();
        assert!(
            failures.is_empty(),
            "cases below threshold: {:?}",
            failures
                .iter()
                .map(|s| format!(
                    "{} (missing {:?}, first@{:?})",
                    s.name, s.missing, s.first_rank
                ))
                .collect::<Vec<_>>()
        );
    }

    /// Not a gate — the tuning loop C2 exists to enable. Run it to pick `alpha`
    /// against the whole fixture set instead of one prompt:
    ///
    /// ```text
    /// cargo test --lib alpha_sweep -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "reporting only; requires live TEI at http://localhost:8081"]
    fn alpha_sweep() {
        let (fx, _config, embedder, store) = harness();

        println!("\n{:<34}alpha → rank of first expected chunk", "case");
        let alphas = [0.6_f32, 0.4, 0.3, 0.2, 0.1, 0.0];
        print!("{:<34}", "");
        for a in alphas {
            print!("{a:>7}");
        }
        println!();

        let mut totals = vec![0usize; alphas.len()];
        for case in &fx.cases {
            print!("{:<34}", case.name);
            for (i, a) in alphas.iter().enumerate() {
                let s = run_case(case, &store, &embedder, *a);
                match s.first_rank {
                    Some(r) => {
                        print!("{r:>7}");
                        totals[i] += r;
                    }
                    None => {
                        print!("{:>7}", "MISS");
                        totals[i] += 999;
                    }
                }
            }
            println!();
        }
        print!("{:<34}", "TOTAL (lower is better)");
        for t in &totals {
            print!("{t:>7}");
        }
        println!("\n");
    }
}
