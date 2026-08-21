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

// The library. This is what a service or a future MCP server consumes.
pub mod config;
pub mod error;
pub mod index;
pub mod vault;

// The CLI. These four are `vault` the command-line tool, not `vault` the
// library: between them they own every `print!`, every stdin read, and the one
// `process::exit`. They are behind the default-on `cli` feature so a consumer
// can leave them — and clap — behind with `default-features = false`, and so
// that "is this the library or the CLI?" is a question the compiler answers
// rather than a convention in a doc comment.
//
// This is what keeps rule 3 honest. It does not make the pipelines print-free
// by itself — that is a property of the pipeline code, checked separately —
// but it means a consumer who opts out cannot even reach a printing entry
// point.
#[cfg(feature = "cli")]
pub mod configure;
#[cfg(feature = "cli")]
pub mod diagnose;
#[cfg(feature = "cli")]
pub mod hook;
#[cfg(feature = "cli")]
pub mod tei;

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
pub use config::{Config, ConfigError};
pub use embed::EmbedError;
pub use error::VaultError;
pub use index::classify::ClassifyError;
pub use index::sync::{Interaction, SyncError, SyncOptions, SyncReport};
pub use index::walk::WalkError;
pub use retrieve::{Context, PlannedQuery, QueryPlan, Retrieval, RouterError, SkipReason};
pub use store::{Hit, StoreError};
pub use types::{DocType, Language};
pub use vault::{QueryPlanner, Vault, VaultStore};
