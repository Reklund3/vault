use std::error::Error;
use std::str::FromStr;

use clap::Args as ClapArgs;

use crate::config::Config;
use crate::embed::{Embedder, StubEmbedder, TeiEmbedder};
use crate::retrieve::{
    PlannedQuery, QueryPlan, ResolvedBackend, RouterOutput, SearchTrace, build_router,
    resolve_backend, search_traced,
};
use crate::store::{SqliteStore, Store};
use crate::types::{DocType, Inventory, Language};

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

    // Opened before the router runs, because the router is grounded with what
    // this store actually holds. `--no-router` still opens it — the trace prints
    // store-derived results either way.
    let db_path = config.db_path()?;
    let store = SqliteStore::open(&db_path, &config)?;
    let inventory = store.inventory()?;

    let (router_status, plan) = if args.no_router {
        (RouterStatus::Bypassed, Some(cli.clone().into_plan()))
    } else {
        let backend = resolve_backend(&config);
        let router = build_router(&config)?;
        let output = router.plan(&args.prompt, &inventory)?;
        plan_for_output(output, backend, &inventory, &cli)
    };

    let alpha = args.alpha.unwrap_or(config.alpha());
    let budget_tokens = config.token_budget() as u32;
    let min_score = config.min_score();
    let max_hits = config.max_hits();
    let used_stub = args.stub;

    // Cheap COUNT over the same filter clause the search arms use. Runs before
    // the header so the trace can say whether the plan above it reached
    // anything; `None` (backend cannot count) prints nothing.
    let filter_reach = match plan.as_ref() {
        Some(p) => {
            let matching = store.count_matching_filters(p)?;
            // Only when the filters caught nothing is a retry coming, and only
            // then is it worth two more counts to describe what it will search.
            let retried_over = if matching == Some(0) {
                let mut relaxed = p.clone();
                relaxed.languages.clear();
                relaxed.doc_types.clear();
                let scope = store.count_matching_filters(&relaxed)?;
                let total = store.count_matching_filters(&QueryPlan {
                    projects: vec![],
                    type_names: vec![],
                    topics: vec![],
                    doc_types: vec![],
                    languages: vec![],
                })?;
                scope.zip(total)
            } else {
                None
            };
            filter_reach(p, matching, retried_over)
        }
        None => FilterReach::Unreported,
    };

    print_header(&TraceHeader {
        prompt: &args.prompt,
        router_status: &router_status,
        router_mode: config.router_mode(),
        plan: plan.as_ref(),
        overrides: &cli,
        alpha,
        budget_tokens,
        min_score,
        max_hits,
        inventory: &inventory,
        filter_reach: &filter_reach,
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

    // The same call `vault hook` makes. Everything below is a view of its
    // result, not a second implementation — see `SearchTrace`.
    let planned = PlannedQuery {
        plan,
        embedding: embedder.embed_query(&args.prompt)?,
        // `diagnose` has no hook `cwd` to resolve; retrieval is unaffected
        // either way, since this only steers domain resolution.
        cwd_project: None,
    };
    let trace = search_traced(&planned, &config, &store, alpha)?;

    print_results(&trace, args.top, budget_tokens, max_hits);
    Ok(())
}

/// What the plan's structural filters actually reached in the store.
///
/// `hybrid_search` silently retries with `doc_types`/`languages` cleared when a
/// filtered pass returns nothing. That rescue is right on the hook path and
/// wrong to hide here: without it on screen the trace prints a filter in the
/// plan and the whole unfiltered corpus underneath, which reads as proof the
/// filter matched.
#[derive(Debug)]
enum FilterReach {
    /// Nothing to say: the plan carries no structural filter, or the backend
    /// does not implement the count.
    Unreported,
    Matched(usize),
    /// The filters selected nothing, so `hybrid_search` cleared
    /// `doc_types`/`languages` and retried. `retried_over` is `(scope, total)`
    /// — how many chunks the retry could actually reach, against the whole
    /// corpus — or `None` when the backend cannot count.
    ///
    /// The retry keeps `projects`, so it is *not* necessarily unfiltered. It is
    /// measured rather than read off the plan because `existing_project_ids`
    /// degrades project names that do not resolve, and a plan naming only
    /// unknown projects retries over everything despite listing a filter.
    Relaxed {
        retried_over: Option<(usize, usize)>,
    },
    /// The filters selected nothing and no retry fired: the relax clears only
    /// `doc_types`/`languages`, so a `projects`-only plan returns empty.
    Nothing,
}

fn filter_reach(
    plan: &QueryPlan,
    matching: Option<usize>,
    retried_over: Option<(usize, usize)>,
) -> FilterReach {
    let has_filter =
        !plan.projects.is_empty() || !plan.doc_types.is_empty() || !plan.languages.is_empty();
    if !has_filter {
        return FilterReach::Unreported;
    }
    match matching {
        None => FilterReach::Unreported,
        // Mirrors the retry condition in `Store::hybrid_search`: it fires for
        // `doc_types`/`languages` only.
        Some(0) if !plan.doc_types.is_empty() || !plan.languages.is_empty() => {
            FilterReach::Relaxed { retried_over }
        }
        Some(0) => FilterReach::Nothing,
        Some(n) => FilterReach::Matched(n),
    }
}

/// Decide what the trace should search, given what the router returned.
///
/// Split out of `run` so it is reachable from tests: `run` itself needs a
/// loaded `Config`, an open store, and a live embedder, so the decision it
/// used to make inline could not be exercised at all.
fn plan_for_output(
    output: RouterOutput,
    backend: ResolvedBackend,
    inventory: &Inventory,
    cli: &Overrides,
) -> (RouterStatus, Option<QueryPlan>) {
    match output {
        // A skip with filter flags on the command line is not a skip. Those
        // flags exist to drive the store when the router's judgement is
        // unhelpful, and the router judging a prompt uninteresting is the case
        // where they matter most. Build the plan from the overrides alone —
        // there is no router plan to merge onto — and leave it unpruned, the
        // same treatment `--no-router` gives them.
        RouterOutput::Skip if !cli.is_empty() => (
            RouterStatus::Skip {
                backend,
                overridden: true,
            },
            Some(cli.clone().into_plan()),
        ),
        RouterOutput::Skip => (
            RouterStatus::Skip {
                backend,
                overridden: false,
            },
            None,
        ),
        RouterOutput::Plan(mut p) => {
            // Prune before merging overrides, never after: an explicit
            // `--languages go` is the operator deliberately probing a filter
            // that matches nothing, and silently discarding it would break
            // the one tool for observing that path.
            p.retain_indexed(inventory);
            (
                RouterStatus::Plan { backend },
                Some(merge_overrides(p, cli)),
            )
        }
    }
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
    Skip {
        backend: ResolvedBackend,
        /// The router said skip, but the operator passed filter flags, so a
        /// search ran anyway. Tracked so the header can say so — printing a
        /// bare `decision: skip` above a page of results reads as a bug in the
        /// tool.
        overridden: bool,
    },
    Plan {
        backend: ResolvedBackend,
    },
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
    /// `defaults.max_hits`, or `None` when uncapped. Shown because it silently
    /// bounds the result set: without it on screen, a cap doing the trimming
    /// looks exactly like a scoring problem.
    max_hits: Option<usize>,
    /// What the store actually holds. Printed because it now *shapes* the plan
    /// twice over — it is rendered into the router's user turn, and it prunes
    /// enum-valid values the router returns anyway. Without it on screen a
    /// pruned filter looks like a router that never proposed one.
    inventory: &'a Inventory,
    /// What the plan's structural filters actually selected. Printed because a
    /// zero here means the store threw the filter away and searched everything.
    filter_reach: &'a FilterReach,
    used_stub: bool,
}

fn backend_label(b: ResolvedBackend) -> &'static str {
    match b {
        ResolvedBackend::Gemma => "Gemma",
        ResolvedBackend::Haiku => "Haiku",
        ResolvedBackend::OpenAiCompat => "OpenAI-compatible",
    }
}

fn print_header(h: &TraceHeader<'_>) {
    println!();
    println!("prompt:    {:?}", h.prompt);
    match h.router_status {
        RouterStatus::Bypassed => println!("router:    bypassed (--no-router)"),
        RouterStatus::Skip {
            backend,
            overridden,
        } => {
            let suffix = if *overridden {
                " (overridden by CLI filters — searching anyway)"
            } else {
                ""
            };
            println!(
                "router:    {} ({}) — decision: skip{}",
                backend_label(*backend),
                h.router_mode,
                suffix
            );
        }
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
    match h.filter_reach {
        FilterReach::Unreported => {}
        FilterReach::Matched(n) => println!("filters:   match {n} chunks"),
        FilterReach::Relaxed { retried_over } => match retried_over {
            // The retry keeps `projects`, so say what it could actually reach
            // rather than calling every relaxed search unfiltered.
            Some((scope, total)) if scope < total => println!(
                "filters:   match 0 chunks — doc_types/languages dropped, \
                 retried over {scope} of {total} chunks (projects filter kept)"
            ),
            Some((_, total)) => println!(
                "filters:   match 0 chunks — doc_types/languages dropped, \
                 retried over all {total} chunks"
            ),
            None => println!("filters:   match 0 chunks — doc_types/languages dropped, retried"),
        },
        FilterReach::Nothing => println!("filters:   match 0 chunks"),
    }
    if h.inventory.is_empty() {
        println!("indexed:   (nothing — router ungrounded, no pruning applied)");
    } else {
        println!("indexed:   projects={:?}", h.inventory.projects);
        println!(
            "           doc_types={:?}  languages={:?}",
            h.inventory
                .doc_types
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>(),
            h.inventory
                .languages
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>(),
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
    match h.max_hits {
        Some(cap) => println!("max_hits:  {cap}"),
        None => println!("max_hits:  uncapped"),
    }
    if h.used_stub {
        println!("embedder:  StubEmbedder (cosine scores are not semantically meaningful)");
    } else {
        println!("embedder:  TeiEmbedder");
    }
}

/// Which limit stopped the selection.
///
/// `select_within_budget` breaks the moment the cap fills, so `kept == cap` is
/// exactly the condition under which `max_hits` — not scoring — did the
/// trimming. Getting this wrong is not cosmetic: `diagnose` exists to explain
/// why chunks were cut, and blaming `min_score/budget` for a cap sends someone
/// tuning alpha after a phantom.
fn trim_cause(kept: usize, max_hits: Option<usize>) -> &'static str {
    match max_hits {
        Some(cap) if kept == cap => "max_hits cap",
        _ => "min_score/budget",
    }
}

fn print_results(trace: &SearchTrace, top: usize, budget_tokens: u32, max_hits: Option<usize>) {
    let sel = &trace.selection;
    let raw_count = trace.raw_count;
    let kept = sel.chunks.len();
    let trimmed = raw_count.saturating_sub(kept);
    println!(
        "hits:      {} returned, {} within budget ({}/{} tokens used){}",
        raw_count,
        kept,
        sel.tokens_used,
        budget_tokens,
        if trimmed > 0 {
            format!(", {trimmed} dropped ({})", trim_cause(kept, max_hits))
        } else {
            String::new()
        }
    );
    // Which block the hook would actually emit. Available now that diagnose runs
    // the same pipeline; previously this tool could not tell you.
    if let Some(f) = &trace.framing {
        let tag = crate::retrieve::safe_tag(&f.tag);
        match crate::retrieve::safe_domain(f.domain.as_deref()) {
            Some(domain) => println!("tag:       <{tag} domain=\"{domain}\">"),
            None => println!("tag:       <{tag}>  (no domain assigned)"),
        }
    }
    println!();

    if sel.chunks.is_empty() {
        if raw_count == 0 {
            println!(
                "(no matches — has the DB been seeded? Run `vault index sync <repo>` once \
                 it's wired up, or seed via integration tests.)"
            );
        } else {
            println!(
                "(all {raw_count} hits dropped by {})",
                trim_cause(0, max_hits)
            );
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

    // ----- trim attribution (code-review finding 1) -----

    /// `select_within_budget` stops the moment the cap fills, so `kept == cap`
    /// is exactly when `max_hits` did the trimming. Naming `min_score/budget`
    /// there sends someone tuning alpha after a problem that is not there.
    #[test]
    fn a_full_cap_is_attributed_to_max_hits() {
        assert_eq!(trim_cause(4, Some(4)), "max_hits cap");
        assert_eq!(trim_cause(0, Some(0)), "max_hits cap");
    }

    /// Under the cap, the cap cannot be what stopped it.
    #[test]
    fn stopping_below_the_cap_is_attributed_to_scoring() {
        assert_eq!(trim_cause(3, Some(10)), "min_score/budget");
    }

    #[test]
    fn without_a_cap_trimming_is_always_scoring() {
        assert_eq!(trim_cause(0, None), "min_score/budget");
        assert_eq!(trim_cause(50, None), "min_score/budget");
    }
    fn cli_overrides() -> Overrides {
        Overrides {
            topics: vec!["auth".into()],
            ..Default::default()
        }
    }

    fn rust_only_inventory() -> Inventory {
        Inventory {
            projects: vec!["vault".into()],
            languages: vec![Language::Rust],
            doc_types: vec![DocType::Meta],
        }
    }

    /// The whole point of `--topics`/`--languages`/... is to drive the store
    /// when the router's judgement is unhelpful. A skip must not throw them
    /// away: `vault diagnose "hi" --topics auth` is an operator deliberately
    /// probing the `auth` topic, and answering "no search ran" leaves them with
    /// no way to reach the store at all short of `--no-router`.
    #[test]
    fn a_skip_still_searches_when_the_operator_supplied_filters() {
        let (status, plan) = plan_for_output(
            RouterOutput::Skip,
            ResolvedBackend::Gemma,
            &rust_only_inventory(),
            &cli_overrides(),
        );

        let plan = plan.expect("CLI overrides must survive a router skip");
        assert_eq!(plan.topics, vec!["auth".to_string()]);
        assert!(
            matches!(
                status,
                RouterStatus::Skip {
                    overridden: true,
                    ..
                }
            ),
            "the header must say the skip was overridden, or it reports \
             `decision: skip` directly above a page of results"
        );
    }

    /// A skip with nothing to override is still a skip.
    #[test]
    fn a_bare_skip_runs_no_search() {
        let (status, plan) = plan_for_output(
            RouterOutput::Skip,
            ResolvedBackend::Gemma,
            &rust_only_inventory(),
            &Overrides::default(),
        );

        assert!(plan.is_none());
        assert!(matches!(
            status,
            RouterStatus::Skip {
                overridden: false,
                ..
            }
        ));
    }

    /// Overrides on a skip are the operator's word, so they bypass
    /// `retain_indexed` exactly as they do on a router-supplied plan and under
    /// `--no-router`. Pruning `go` out of an explicit `--languages go` would
    /// remove the only way to watch that filter match nothing.
    #[test]
    fn a_skip_override_is_not_pruned_against_the_inventory() {
        let overrides = Overrides {
            languages: vec![Language::Go],
            ..Default::default()
        };

        let (_, plan) = plan_for_output(
            RouterOutput::Skip,
            ResolvedBackend::Gemma,
            &rust_only_inventory(),
            &overrides,
        );

        assert_eq!(
            plan.expect("overrides present").languages,
            vec![Language::Go],
            "an explicit --languages must reach the store unpruned"
        );
    }

    /// The router-plan path is unchanged: pruned against the inventory, then
    /// overridden.
    #[test]
    fn a_router_plan_is_pruned_then_overridden() {
        let mut routed = full_plan();
        routed.languages = vec![Language::Rust, Language::Go];

        let (status, plan) = plan_for_output(
            RouterOutput::Plan(routed),
            ResolvedBackend::Gemma,
            &rust_only_inventory(),
            &cli_overrides(),
        );

        let plan = plan.expect("a plan was returned");
        assert_eq!(plan.languages, vec![Language::Rust], "go is not indexed");
        assert_eq!(plan.topics, vec!["auth".to_string()], "override applied");
        assert!(matches!(status, RouterStatus::Plan { .. }));
    }
    /// A filter that selected nothing must be called out, together with what
    /// the retry could actually reach — otherwise the results printed under the
    /// plan look like the plan produced them.
    #[test]
    fn a_filter_matching_no_chunks_is_reported_as_relaxed() {
        let mut plan = empty_plan();
        plan.languages = vec![Language::Helm];

        assert!(matches!(
            filter_reach(&plan, Some(0), Some((705, 705))),
            FilterReach::Relaxed {
                retried_over: Some((705, 705))
            }
        ));
    }

    /// The relax clears `doc_types`/`languages` and **keeps `projects`**, so a
    /// retry under a project filter is not unfiltered. Reporting it as such was
    /// the bug: with one project indexed the two are indistinguishable, and
    /// with two the trace claimed a scope it never searched.
    #[test]
    fn a_relax_under_a_project_filter_is_not_reported_as_unfiltered() {
        let mut plan = empty_plan();
        plan.projects = vec!["vault".into()];
        plan.languages = vec![Language::Helm];

        let reach = filter_reach(&plan, Some(0), Some((631, 705)));
        match reach {
            FilterReach::Relaxed {
                retried_over: Some((scope, total)),
            } => {
                assert!(scope < total, "the surviving project filter must show");
            }
            other => panic!("expected a scoped relax, got {other:?}"),
        }
    }

    /// `hybrid_search` relaxes `doc_types`/`languages` only. A `projects`-only
    /// plan that reaches zero returns an empty result instead, so claiming a
    /// relax there would describe a retry that never ran.
    #[test]
    fn a_projects_only_filter_matching_nothing_is_not_a_relax() {
        let mut plan = empty_plan();
        plan.projects = vec!["never-synced".into()];

        assert!(matches!(
            filter_reach(&plan, Some(0), None),
            FilterReach::Nothing
        ));
    }

    #[test]
    fn a_filter_that_selects_chunks_reports_the_count() {
        let mut plan = empty_plan();
        plan.languages = vec![Language::Rust];

        assert!(matches!(
            filter_reach(&plan, Some(631), None),
            FilterReach::Matched(631)
        ));
    }

    /// Nothing to report when there is no structural filter, or when the
    /// backend cannot count — silence beats a fabricated reassurance.
    #[test]
    fn nothing_is_reported_without_a_filter_or_a_count() {
        assert!(matches!(
            filter_reach(&empty_plan(), Some(0), None),
            FilterReach::Unreported
        ));
        let mut plan = empty_plan();
        plan.languages = vec![Language::Rust];
        assert!(matches!(
            filter_reach(&plan, None, None),
            FilterReach::Unreported
        ));
    }
}
