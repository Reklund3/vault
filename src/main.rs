mod config;
mod diagnose;
mod embed;
mod hook;
mod index;
mod retrieve;
mod store;
mod tei;
mod types;
mod parse;
mod util;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vault", about = "vault: project context for Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Test retrieval against the configured DB and print ranked hits with component scores.
    Diagnose(diagnose::Args),
    /// Pre-prompt hook for Claude Code (UserPromptSubmit). Reads JSON on stdin,
    /// emits a context block on stdout. Always exits 0 — fails open.
    Hook,
    /// Manage the local TEI embeddings server (start | stop | status | logs).
    Tei {
        #[command(subcommand)]
        command: tei::TeiCommand,
    },
    /// Predict what `vault index sync` would do without touching anything.
    /// Walks the repo, reports walk + cache hits + would-classify count + cost
    /// estimate. The full `index sync` subcommand lands in 14.8; this is a smoke
    /// entry so the orchestrator runs against real data before the slice is done.
    IndexSyncDryRun {
        repo: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Diagnose(args) => {
            if let Err(e) = diagnose::run(args) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Command::Hook => hook::run(),
        Command::Tei { command } => {
            if let Err(e) = tei::run(command) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Command::IndexSyncDryRun { repo, name } => {
            if let Err(e) = run_index_sync_dry_run(repo, name) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn run_index_sync_dry_run(
    repo: PathBuf,
    name: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::load()?;
    let opts = index::sync::SyncOptions {
        repo,
        explicit_name: name,
        dry_run: true,
    };
    let report = index::sync::run_sync(opts, &config)?;
    println!("{report:#?}");
    Ok(())
}
