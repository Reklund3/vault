use std::error::Error;
use std::str::FromStr;

use clap::Args as ClapArgs;

use crate::config::Config;
use crate::embed::{Embedder, StubEmbedder, TeiEmbedder};
use crate::retrieve::budget::{self, BudgetedSelection};
use crate::retrieve::{QueryPlan, ResolvedBackend, RouterOutput, build_router, resolve_backend};
use crate::store::{SqliteStore, Store};
use crate::types::{DocType, Language};

type CliResult = Result<(), Box<dyn Error + Send + Sync>>;

#[derive(ClapArgs)]
pub struct Args {
    /// User prompt to test retrieval against.
    prompt: String,

    /// Override the router's `projects` list. Replaces (not merges with) the
    /// router's value when non-empty.
    #[arg(long, value_delimiter = ',')]
    projects: Vec<String>,

    /// Override the router's `type_names` list.
    #[arg(long = "type-names", value_delimiter = ',')]
    type_names: Vec<String>,

    /// Override the router's `topics` list.
    #[arg(long, value_delimiter = ',')]
    topics: Vec<String>,

    /// Override the router's `doc_types` list: contract|plan|convention|meta.
    #[arg(long = "doc-types", value_delimiter = ',')]
    doc_types: Vec<String>,

    /// Override the router's `languages` list: go|rust|scala|proto|openapi|helm|markdown|unknown.
    #[arg(long, value_delimiter = ',')]
    languages: Vec<String>,

    /// BM25/cosine alpha override. Defaults to config defaults.alpha.
    #[arg(long)]
    alpha: Option<f32>,

    /// Limit on results to display.
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// Use the deterministic stub embedder instead of TEI.
    /// Cosine scores will be meaningless — only useful for plumbing checks.
    #[arg(long)]
    stub: bool,

    /// Skip the router entirely and build the QueryPlan from CLI flags alone.
    /// Useful for isolating store behavior from routing.
    #[arg(long)]
    no_router: bool,
}

pub fn run(args: Args) -> CliResult {
    let config = Config::load()?;

    let embedder: Box<dyn Embedder> = if args.stub {
        Box::new(StubEmbedder::from_config(&config))
    } else {
        let tei = TeiEmbedder::from_config(&config)?;
        tei.verify_against_server()?;
        Box::new(tei)
    };

    let cli =
        Overrides::from_args(&args).map_err(|e| -> Box<dyn Error + Send + Sync> { e.into() })?;

    let (router_status, plan) = if args.no_router {
        (RouterStatus::Bypassed, Some(cli.clone().into_plan()))
    } else {
        let backend = resolve_backend(&config);
        let router = build_router(&config)?;
        match router.plan(&args.prompt)? {
            RouterOutput::Skip => (RouterStatus::Skip { backend }, None),
            RouterOutput::Plan(p) => (
                RouterStatus::Plan { backend },
                Some(merge_overrides(p, &cli)),
            ),
        }
    };

    let alpha = args.alpha.unwrap_or(config.alpha());
    let budget_tokens = config.token_budget() as u32;
    let min_score = config.min_score();
    let used_stub = args.stub;

    print_header(&TraceHeader {
        prompt: &args.prompt,
        router_status: &router_status,
        router_mode: config.router_mode(),
        plan: plan.as_ref(),
        overrides: &cli,
        alpha,
        budget_tokens,
        min_score,
        used_stub,
    });

    let plan = match plan {
        Some(p) => p,
        None => {
            println!();
            println!("(router judged no retrieval needed — no search ran)");
            return Ok(());
        }
    };

    let db_path = config.db_path()?;
    let store = SqliteStore::open(&db_path, &config)?;

    let query_emb = embedder.embed_query(&args.prompt)?;
    let raw_hits = store.hybrid_search(&plan, &query_emb, alpha)?;
    let raw_count = raw_hits.len();
    let selection = budget::select_within_budget(raw_hits, budget_tokens, min_score);

    print_results(&selection, raw_count, args.top, budget_tokens);
    Ok(())
}

fn parse_list<T: FromStr<Err = String>>(specs: &[String]) -> Result<Vec<T>, String> {
    specs.iter().map(|s| s.parse()).collect()
}

/// CLI-supplied filter overrides, after parsing into typed values. An empty
/// `Vec` means "don't override this field"; a non-empty `Vec` replaces whatever
/// the router proposed for that field.
#[derive(Debug, Clone, Default)]
struct Overrides {
    projects: Vec<String>,
    type_names: Vec<String>,
    topics: Vec<String>,
    doc_types: Vec<DocType>,
    languages: Vec<Language>,
}

impl Overrides {
    fn from_args(args: &Args) -> Result<Self, String> {
        Ok(Self {
            projects: args.projects.clone(),
            type_names: args.type_names.clone(),
            topics: args.topics.clone(),
            doc_types: parse_list::<DocType>(&args.doc_types)
                .map_err(|e| format!("--doc-types: {e}"))?,
            languages: parse_list::<Language>(&args.languages)
                .map_err(|e| format!("--languages: {e}"))?,
        })
    }

    fn is_empty(&self) -> bool {
        self.projects.is_empty()
            && self.type_names.is_empty()
            && self.topics.is_empty()
            && self.doc_types.is_empty()
            && self.languages.is_empty()
    }

    /// Build a QueryPlan from overrides alone — for `--no-router` mode.
    fn into_plan(self) -> QueryPlan {
        QueryPlan {
            projects: self.projects,
            type_names: self.type_names,
            topics: self.topics,
            doc_types: self.doc_types,
            languages: self.languages,
        }
    }
}

/// Replace any field of `plan` whose corresponding override is non-empty. An
/// empty override leaves the router's value untouched.
fn merge_overrides(mut plan: QueryPlan, overrides: &Overrides) -> QueryPlan {
    if !overrides.projects.is_empty() {
        plan.projects = overrides.projects.clone();
    }
    if !overrides.type_names.is_empty() {
        plan.type_names = overrides.type_names.clone();
    }
    if !overrides.topics.is_empty() {
        plan.topics = overrides.topics.clone();
    }
    if !overrides.doc_types.is_empty() {
        plan.doc_types = overrides.doc_types.clone();
    }
    if !overrides.languages.is_empty() {
        plan.languages = overrides.languages.clone();
    }
    plan
}

enum RouterStatus {
    Bypassed,
    Skip { backend: ResolvedBackend },
    Plan { backend: ResolvedBackend },
}

struct TraceHeader<'a> {
    prompt: &'a str,
    router_status: &'a RouterStatus,
    /// The configured `[router].mode` (`auto`/`gemma`/`haiku`) — shown verbatim
    /// so a forced backend isn't mislabeled as auto-resolved.
    router_mode: &'a str,
    plan: Option<&'a QueryPlan>,
    overrides: &'a Overrides,
    alpha: f32,
    budget_tokens: u32,
    min_score: f32,
    used_stub: bool,
}

fn backend_label(b: ResolvedBackend) -> &'static str {
    match b {
        ResolvedBackend::Gemma => "Gemma",
        ResolvedBackend::Haiku => "Haiku",
    }
}

fn print_header(h: &TraceHeader<'_>) {
    println!();
    println!("prompt:    {:?}", h.prompt);
    match h.router_status {
        RouterStatus::Bypassed => println!("router:    bypassed (--no-router)"),
        RouterStatus::Skip { backend } => println!(
            "router:    {} ({}) — decision: skip",
            backend_label(*backend),
            h.router_mode
        ),
        RouterStatus::Plan { backend } => {
            println!("router:    {} ({})", backend_label(*backend), h.router_mode)
        }
    }
    if let Some(plan) = h.plan {
        println!(
            "plan:      projects={:?}  type_names={:?}  topics={:?}",
            plan.projects, plan.type_names, plan.topics,
        );
        let doc_types: Vec<&str> = plan.doc_types.iter().map(|d| d.as_str()).collect();
        let languages: Vec<&str> = plan.languages.iter().map(|l| l.as_str()).collect();
        println!(
            "           doc_types={:?}  languages={:?}",
            doc_types, languages,
        );
    }
    if h.overrides.is_empty() {
        println!("overrides: (none)");
    } else {
        println!(
            "overrides: projects={:?}  type_names={:?}  topics={:?}  doc_types={:?}  languages={:?}",
            h.overrides.projects,
            h.overrides.type_names,
            h.overrides.topics,
            h.overrides
                .doc_types
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>(),
            h.overrides
                .languages
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>(),
        );
    }
    println!("alpha:     {}", h.alpha);
    println!(
        "budget:    {} tokens (min_score {})",
        h.budget_tokens, h.min_score
    );
    if h.used_stub {
        println!("embedder:  StubEmbedder (cosine scores are not semantically meaningful)");
    } else {
        println!("embedder:  TeiEmbedder");
    }
}

fn print_results(sel: &BudgetedSelection, raw_count: usize, top: usize, budget_tokens: u32) {
    let kept = sel.chunks.len();
    let trimmed = raw_count.saturating_sub(kept);
    println!(
        "hits:      {} returned, {} within budget ({}/{} tokens used){}",
        raw_count,
        kept,
        sel.tokens_used,
        budget_tokens,
        if trimmed > 0 {
            format!(", {trimmed} dropped (min_score/budget)")
        } else {
            String::new()
        }
    );
    println!();

    if sel.chunks.is_empty() {
        if raw_count == 0 {
            println!(
                "(no matches — has the DB been seeded? Run `vault index sync <repo>` once \
                 it's wired up, or seed via integration tests.)"
            );
        } else {
            println!("(all {raw_count} hits dropped by min_score or token budget)");
        }
        return;
    }

    let mut cumulative: u32 = 0;
    for (i, h) in sel.chunks.iter().take(top).enumerate() {
        cumulative += h.token_est;
        println!(
            "#{:<2} bm25={:.3}  cos={:.3}  final={:.3}  ~{} tok  [cumulative {}]",
            i + 1,
            h.bm25_score,
            h.cosine_score,
            h.final_score,
            h.token_est,
            cumulative,
        );
        println!(
            "    {} [{}]  chunk_id={}",
            h.label,
            h.doc_type.as_str(),
            h.chunk_id,
        );
        let snippet: String = h.content.chars().take(160).collect();
        let suffix = if h.content.chars().count() > 160 {
            "…"
        } else {
            ""
        };
        println!("    {snippet}{suffix}");
        println!();
    }
    if kept > top {
        println!("(showing top {top} of {kept} within-budget hits)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_plan() -> QueryPlan {
        QueryPlan {
            projects: vec![],
            type_names: vec![],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        }
    }

    fn full_plan() -> QueryPlan {
        QueryPlan {
            projects: vec!["router-pick".into()],
            type_names: vec!["Router".into()],
            topics: vec!["routing".into()],
            doc_types: vec![DocType::Meta],
            languages: vec![Language::Rust],
        }
    }

    #[test]
    fn merge_overrides_empty_leaves_plan_untouched() {
        let plan = full_plan();
        let merged = merge_overrides(plan.clone(), &Overrides::default());
        assert_eq!(merged.projects, plan.projects);
        assert_eq!(merged.type_names, plan.type_names);
        assert_eq!(merged.topics, plan.topics);
        assert_eq!(merged.doc_types, plan.doc_types);
        assert_eq!(merged.languages, plan.languages);
    }

    #[test]
    fn merge_overrides_replaces_only_non_empty_fields() {
        let overrides = Overrides {
            projects: vec!["cli-pick".into()],
            doc_types: vec![DocType::Convention],
            ..Overrides::default()
        };
        let merged = merge_overrides(full_plan(), &overrides);
        // Replaced
        assert_eq!(merged.projects, vec!["cli-pick".to_string()]);
        assert_eq!(merged.doc_types, vec![DocType::Convention]);
        // Untouched from router
        assert_eq!(merged.type_names, vec!["Router".to_string()]);
        assert_eq!(merged.topics, vec!["routing".to_string()]);
        assert_eq!(merged.languages, vec![Language::Rust]);
    }

    #[test]
    fn merge_overrides_onto_empty_router_plan() {
        let overrides = Overrides {
            type_names: vec!["CliType".into()],
            languages: vec![Language::Proto],
            ..Overrides::default()
        };
        let merged = merge_overrides(empty_plan(), &overrides);
        assert_eq!(merged.type_names, vec!["CliType".to_string()]);
        assert_eq!(merged.languages, vec![Language::Proto]);
        assert!(merged.projects.is_empty());
        assert!(merged.doc_types.is_empty());
    }

    #[test]
    fn overrides_is_empty_reports_default_as_empty() {
        assert!(Overrides::default().is_empty());
    }

    #[test]
    fn overrides_is_empty_false_when_any_field_set() {
        let o = Overrides {
            topics: vec!["x".into()],
            ..Overrides::default()
        };
        assert!(!o.is_empty());
    }
}
