mod config;
mod diagnose;
mod embed;
mod hook;
mod index;
mod retrieve;
mod store;
mod types;
mod parse;
mod util;

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
    }
}
