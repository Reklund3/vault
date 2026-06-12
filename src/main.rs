mod config;
mod diagnose;
mod embed;
mod hook;
mod index;
mod parse;
mod retrieve;
mod store;
mod tei;
mod types;
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
    /// Index management (sync, …).
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
}

#[derive(Subcommand)]
enum IndexCommand {
    /// Walk a repo, classify each file, parse + embed, and upsert chunks into the store.
    /// With `--dry-run`, only walks and reports counters — no remote calls, no DB writes.
    Sync {
        /// Path to the repo to index.
        repo: PathBuf,
        /// Project name override (default: canonical path's last component).
        #[arg(long)]
        name: Option<String>,
        /// Walk + cache-lookup only. Skips TEI, classifier, and store writes.
        #[arg(long)]
        dry_run: bool,
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
        Command::Index { command } => match command {
            IndexCommand::Sync {
                repo,
                name,
                dry_run,
            } => {
                if let Err(e) = run_index_sync(repo, name, dry_run) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        },
    }
}

fn run_index_sync(
    repo: PathBuf,
    name: Option<String>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::load()?;
    let opts = index::sync::SyncOptions {
        repo,
        explicit_name: name,
        dry_run,
    };
    let report = index::sync::run_sync(opts, &config)?;
    print!("{}", index::sync::format_report(&report));
    Ok(())
}
