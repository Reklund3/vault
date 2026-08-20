//! `vault` — project context injection for Claude Code.
//!
//! This crate is consumed two ways: by the `vault` binary (see `main.rs`), and
//! as a library by other processes that want the retrieval or indexing
//! pipelines directly.
//!
//! The public surface is deliberately narrow and is still being curated (the
//! lib-split is a staged migration; this is a waypoint, not the destination).
//! Modules kept private at the crate root are implementation detail and remain
//! reachable crate-internally via `crate::`.
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
pub mod vault;

// Private at the root: implementation detail, still crate-visible via `crate::`.
mod embed;
mod parse;
mod retrieve;
mod store;
mod types;
mod util;

// Types named by the public API, re-exported so consumers can reach them
// without the modules that define them becoming public.
//
// A public field whose type is unnameable is only half-public: `Context.hits`
// can be read, but without `Hit` in scope a consumer cannot write a function
// that takes one or store it in a struct. Everything reachable through a public
// field belongs here — `tests/public_api.rs` names each of them so a regression
// fails the build rather than surfacing as an awkward downstream workaround.
pub use embed::EmbedError;
pub use error::VaultError;
pub use retrieve::{Context, PlannedQuery, QueryPlan, Retrieval, RouterError, SkipReason};
pub use store::{Hit, StoreError};
pub use types::{DocType, Language};
pub use vault::{QueryPlanner, Vault, VaultStore};
