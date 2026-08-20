//! `vault` — project context injection for Claude Code.
//!
//! This crate is consumed two ways: by the `vault` binary (see `main.rs`), and
//! as a library by other processes that want the retrieval or indexing
//! pipelines directly.
//!
//! The public surface is deliberately narrow and is curated in
//! `docs/lib-split-plan.md`. Modules kept private at the crate root are
//! implementation detail and still reachable crate-internally via `crate::`.
//!
//! Two invariants matter to anything built on this crate:
//!
//! - **Fail-open is CLI policy, not library policy.** `vault hook` exits 0 on
//!   every error path; library entry points return `Result` and let the caller
//!   decide.
//! - **The library never reads stdin or writes stdout.** All CLI I/O lives in
//!   `main.rs`, `configure`, `diagnose`, and `tei`. A future stdio consumer
//!   (an MCP server) uses both streams for protocol framing.

// Public: reached by the CLI today, and the starting point for the curated API.
pub mod config;
pub mod configure;
pub mod diagnose;
pub mod error;
pub mod hook;
pub mod index;
pub mod tei;

// Private at the root: implementation detail, still crate-visible via `crate::`.
mod embed;
mod parse;
mod retrieve;
mod store;
mod types;
mod util;

// The error types `VaultError` names are re-exported here so consumers can match
// on them without the modules that define them becoming public.
pub use embed::EmbedError;
pub use error::VaultError;
pub use retrieve::RouterError;
pub use store::StoreError;
