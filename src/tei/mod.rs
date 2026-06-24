//! `vault tei start | stop | status | logs` — manage the local TEI embeddings
//! server as a detached child process. TEI runs as a separate process by design
//! (see `docs/security.md` → "Process boundaries are defense-in-depth"); this
//! subcommand group hides that operational surface so daily use is one binary.

mod launcher;

use clap::Subcommand;

use crate::config::Config;

pub use launcher::LauncherError;

#[derive(Subcommand)]
pub enum TeiCommand {
    /// Spawn TEI from `[embeddings].launcher_cmd`, detached, with a scrubbed
    /// environment. No-op if TEI is already reachable.
    Start,
    /// Stop the TEI instance recorded in `~/.vault/tei.pid`.
    Stop,
    /// Report endpoint reachability, the pidfile, and the configured launcher.
    Status,
    /// Print the tail of `~/.vault/tei.log`.
    Logs,
}

/// Dispatch a `vault tei <cmd>`. Loads config once, then delegates. Errors are
/// surfaced to the caller (main.rs) which prints them and exits non-zero —
/// unlike the hook, the `tei` subcommands are interactive and may fail loudly.
pub fn run(cmd: TeiCommand) -> Result<(), LauncherError> {
    let config = Config::load()?;
    match cmd {
        TeiCommand::Start => launcher::start(&config),
        TeiCommand::Stop => launcher::stop(&config),
        TeiCommand::Status => launcher::status(&config),
        TeiCommand::Logs => launcher::logs(&config),
    }
}
