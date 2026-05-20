use std::error::Error;
use std::str::FromStr;

use clap::Args as ClapArgs;

use crate::config::Config;
use crate::embed::{Embedder, StubEmbedder, TeiEmbedder};
use crate::retrieve::QueryPlan;
use crate::store::{Hit, SqliteStore, Store};
use crate::types::{DocType, Language};

type CliResult = Result<(), Box<dyn Error + Send + Sync>>;

#[derive(ClapArgs)]
pub struct Args {
    /// User prompt to test retrieval against.
    prompt: String,

    /// Project names to filter by.
    #[arg(long, value_delimiter = ',')]
    projects: Vec<String>,

    /// Type or symbol names for BM25 keyword match.
    #[arg(long = "type-names", value_delimiter = ',')]
    type_names: Vec<String>,

    /// Topic keywords for BM25 match.
    #[arg(long, value_delimiter = ',')]
    topics: Vec<String>,

    /// Document type filter: contract|plan|convention|meta.
    #[arg(long = "doc-types", value_delimiter = ',')]
    doc_types: Vec<String>,

    /// Language filter: go|rust|scala|proto|openapi|helm|markdown|unknown.
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

    let db_path = config.db_path()?;
    let store = SqliteStore::open(&db_path, &config)?;

    let doc_types = parse_list::<DocType>(&args.doc_types).map_err(|e| format!("--doc-types: {e}"))?;
    let languages = parse_list::<Language>(&args.languages).map_err(|e| format!("--languages: {e}"))?;

    let plan = QueryPlan {
        projects: args.projects,
        type_names: args.type_names,
        topics: args.topics,
        doc_types,
        languages,
    };

    let query_emb = embedder.embed_query(&args.prompt)?;
    let alpha = args.alpha.unwrap_or(config.alpha());

    let mut hits = store.hybrid_search(&plan, &query_emb, alpha)?;
    hits.truncate(args.top);

    print_hits(&args.prompt, alpha, args.stub, &hits);
    Ok(())
}

fn parse_list<T: FromStr<Err = String>>(specs: &[String]) -> Result<Vec<T>, String> {
    specs.iter().map(|s| s.parse()).collect()
}

fn print_hits(prompt: &str, alpha: f32, used_stub: bool, hits: &[Hit]) {
    println!();
    println!("prompt:    {prompt:?}");
    println!("alpha:     {alpha}");
    if used_stub {
        println!("embedder:  StubEmbedder (cosine scores are not semantically meaningful)");
    } else {
        println!("embedder:  TeiEmbedder");
    }
    println!("hits:      {}", hits.len());
    println!();

    if hits.is_empty() {
        println!(
            "(no matches — has the DB been seeded? `vault index sync` is not yet implemented; \
             seed via tests or wait for parsers.)"
        );
        return;
    }

    for (i, h) in hits.iter().enumerate() {
        println!(
            "#{:<2} bm25={:.3}  cos={:.3}  final={:.3}",
            i + 1,
            h.bm25_score,
            h.cosine_score,
            h.final_score,
        );
        println!(
            "    {} [{}]  chunk_id={}  ~{} tok",
            h.label,
            h.doc_type.as_str(),
            h.chunk_id,
            h.token_est,
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
}
