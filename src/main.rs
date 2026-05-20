mod config;
mod diagnose;
mod embed;
mod retrieve;
mod store;
mod types;
mod parse;

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
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Diagnose(args) => diagnose::run(args),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
