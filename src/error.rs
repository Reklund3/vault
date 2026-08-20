//! `VaultError` — the error type library consumers see.
//!
//! Vault has always had per-layer errors (`ConfigError`, `RouterError`,
//! `EmbedError`, `StoreError`). What it lacked was one type at the *boundary*,
//! so a caller could tell a router timeout from a store failure without parsing
//! strings.
//!
//! This is that type, and it keeps two concerns apart:
//!
//! - **The library reports what failed, with the real source attached.** Every
//!   variant wraps the underlying error as a `#[source]`, so a consumer can walk
//!   the chain.
//! - **The hook decides what to do about it.** `vault hook` maps a `VaultError`
//!   onto its telemetry `Stage`, truncates the detail for `hook.log`, and exits
//!   0 regardless. That fail-open behaviour is CLI policy and stays in `hook/` —
//!   a library caller (a service, or a future MCP server) gets the `Err` and
//!   decides for itself.
//!
//! The variants deliberately mirror the pipeline's failure points in execution
//! order, which is what lets the hook derive its `Stage` from the error instead
//! of naming one at each call site.

use crate::config::ConfigError;
use crate::embed::EmbedError;
use crate::index::sync::SyncError;
use crate::retrieve::RouterError;
use crate::store::StoreError;

/// A failure somewhere in the retrieval pipeline, tagged by the step that hit it.
///
/// Two steps can produce the same underlying error type — `RouterBuild` and
/// `RouterPlan` both carry a `RouterError`, `DbOpen` and `Query` both carry a
/// `StoreError` — so the variants distinguish *when* it happened. That is the
/// information the hook's telemetry needs and a string could not carry.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Loading `vault.toml`, or resolving a path derived from it.
    #[error("config: {0}")]
    Config(#[source] ConfigError),

    /// Constructing the router (bad mode, missing API key).
    #[error("router construction failed: {0}")]
    RouterBuild(#[source] RouterError),

    /// Constructing the embedder client.
    #[error("embedder construction failed: {0}")]
    EmbedderBuild(#[source] EmbedError),

    /// Opening or migrating the store, including the embedding model/dim lock.
    #[error("opening the store failed: {0}")]
    DbOpen(#[source] StoreError),

    /// The router failed to produce a query plan (timeout, transport, bad reply).
    #[error("router planning failed: {0}")]
    RouterPlan(#[source] RouterError),

    /// Embedding the prompt failed — typically TEI being unreachable.
    #[error("embedding the query failed: {0}")]
    EmbedQuery(#[source] EmbedError),

    /// The hybrid FTS5 + vector query failed.
    #[error("store query failed: {0}")]
    Query(#[source] StoreError),

    /// Indexing failed. Unlike every variant above, nothing on the hook's
    /// retrieval path can produce this one — it exists for `Vault::sync`, whose
    /// callers are the CLI and library consumers, never `vault hook`.
    #[error("sync failed: {0}")]
    Sync(#[source] SyncError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    /// The point of the type: the underlying error is *attached*, not
    /// stringified. A consumer can reach it and match on it, which is exactly
    /// what the hook's truncated `detail` string cannot offer.
    #[test]
    fn the_underlying_error_stays_reachable_as_a_source() {
        let err = VaultError::DbOpen(StoreError::Backend("disk gone".into()));

        let source = err.source().expect("source must be attached");
        assert!(
            source.to_string().contains("disk gone"),
            "source text lost: {source}"
        );

        // And the concrete type survives, so a caller can match on it.
        match &err {
            VaultError::DbOpen(StoreError::Backend(msg)) => assert_eq!(msg, "disk gone"),
            other => panic!("wrong variant: {other}"),
        }
    }

    /// Same source type, different pipeline step — the variant is what carries
    /// that distinction, and it is what the hook turns into a telemetry stage.
    #[test]
    fn variants_distinguish_when_the_same_error_type_occurred() {
        let build = VaultError::RouterBuild(RouterError::Transport("x".into()));
        let plan = VaultError::RouterPlan(RouterError::Transport("x".into()));

        assert!(matches!(build, VaultError::RouterBuild(_)));
        assert!(matches!(plan, VaultError::RouterPlan(_)));
        assert_ne!(build.to_string(), plan.to_string());
    }
}
