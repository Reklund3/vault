//! The library facade: `QueryPlanner`, `VaultStore`, and `Vault`.
//!
//! Three types rather than one, because the two halves of retrieval have
//! genuinely different properties and collapsing them would cost concurrency:
//!
//! - [`QueryPlanner`] is the network-bound half — a router call under its
//!   timeout, then an embedding call. It touches no database, holds no
//!   connection, and is `Send + Sync`, so it can be shared across threads or
//!   tasks behind an `Arc` with no locking at all.
//! - [`VaultStore`] owns the SQLite connection. It is `Send` but **not** `Sync`
//!   (rusqlite's `Connection` is not), so a concurrent caller needs one per
//!   worker or a lock around it. The work it does is milliseconds of SQLite.
//! - [`Vault`] pairs them for callers that just want an answer.
//!
//! The split is the point. If a single `Vault` held the connection and did the
//! whole pipeline behind one lock, every concurrent request would serialise
//! behind a multi-second router timeout. Keeping the planner shareable means
//! only the store half is ever contended.

use crate::config::Config;
use crate::embed::{Embedder, TeiEmbedder};
use crate::error::VaultError;
use crate::retrieve::{self, PlannedQuery, QueryPlan, Retrieval, Router, RouterOutput, SkipReason};
use crate::store::{SqliteStore, Store};

/// The network-bound half of retrieval: decide what to look for, and vectorise
/// the prompt. Shareable across threads.
pub struct QueryPlanner {
    router: Box<dyn Router + Send + Sync>,
    embedder: Box<dyn Embedder + Send + Sync>,
}

impl QueryPlanner {
    /// Build the configured router and embedder.
    pub fn new(config: &Config) -> Result<Self, VaultError> {
        let router = retrieve::build_router(config).map_err(VaultError::RouterBuild)?;
        let embedder = TeiEmbedder::from_config(config).map_err(VaultError::EmbedderBuild)?;
        Ok(Self {
            router,
            embedder: Box::new(embedder),
        })
    }

    /// Inject backends directly. Test-only on purpose: production always builds
    /// the configured backends via `new`/`open`, so a stub can never reach a
    /// real run — the same compiler-enforced boundary the router and classifier
    /// stubs rely on. Un-gate it if a consumer ever needs real injection.
    #[cfg(test)]
    pub(crate) fn from_parts(
        router: Box<dyn Router + Send + Sync>,
        embedder: Box<dyn Embedder + Send + Sync>,
    ) -> Self {
        Self { router, embedder }
    }

    /// Which backend the router resolved to — `"gemma"`, `"haiku"`, … Stable
    /// identity for telemetry, so a caller never has to re-probe to find out.
    pub fn backend(&self) -> &'static str {
        self.router.name()
    }

    /// The router step alone. `Ok(None)` means the router judged the prompt
    /// self-contained.
    ///
    /// Exposed separately from [`embed_query`](Self::embed_query) so a caller
    /// can time the two independently — `vault hook` records them as distinct
    /// fields, and a single combined call would flatten that.
    pub fn route(&self, prompt: &str) -> Result<Option<QueryPlan>, VaultError> {
        match self.router.plan(prompt).map_err(VaultError::RouterPlan)? {
            RouterOutput::Skip => Ok(None),
            RouterOutput::Plan(plan) => Ok(Some(plan)),
        }
    }

    /// The embedding step alone.
    pub fn embed_query(&self, prompt: &str) -> Result<Vec<f32>, VaultError> {
        self.embedder
            .embed_query(prompt)
            .map_err(VaultError::EmbedQuery)
    }

    /// Both steps, for callers that do not need the seam between them.
    pub fn plan(&self, prompt: &str) -> Result<Option<PlannedQuery>, VaultError> {
        let Some(plan) = self.route(prompt)? else {
            return Ok(None);
        };
        let embedding = self.embed_query(prompt)?;
        Ok(Some(PlannedQuery { plan, embedding }))
    }
}

/// The store-bound half. Owns the connection; `Send` but not `Sync`.
pub struct VaultStore {
    store: Box<dyn Store + Send>,
    config: Config,
}

impl VaultStore {
    /// Open (creating and migrating if needed) the store at the configured path.
    pub fn open(config: &Config) -> Result<Self, VaultError> {
        let db_path = config.db_path().map_err(VaultError::Config)?;
        let store = SqliteStore::open(&db_path, config).map_err(VaultError::DbOpen)?;
        Ok(Self {
            store: Box::new(store),
            config: config.clone(),
        })
    }

    /// Inject backends directly. Test-only on purpose: production always builds
    /// the configured backends via `new`/`open`, so a stub can never reach a
    /// real run — the same compiler-enforced boundary the router and classifier
    /// stubs rely on. Un-gate it if a consumer ever needs real injection.
    #[cfg(test)]
    pub(crate) fn from_store(store: Box<dyn Store + Send>, config: Config) -> Self {
        Self { store, config }
    }

    /// Run a planned query. This is the only segment a concurrent caller has to
    /// serialise, and it is milliseconds of SQLite — the expensive network work
    /// already happened in [`QueryPlanner`].
    pub fn search(&self, planned: &PlannedQuery) -> Result<Retrieval, VaultError> {
        retrieve::search(planned, &self.config, self.store.as_ref())
    }
}

/// Planner plus store, for callers that want one call rather than two phases.
///
/// A concurrent consumer should generally hold the two separately — an
/// `Arc<QueryPlanner>` shared freely, and a `VaultStore` per worker — so the
/// router call happens outside any lock. This type is the single-threaded
/// convenience, and it is what `vault hook` uses.
pub struct Vault {
    planner: QueryPlanner,
    store: VaultStore,
}

impl Vault {
    pub fn open(config: &Config) -> Result<Self, VaultError> {
        Ok(Self {
            planner: QueryPlanner::new(config)?,
            store: VaultStore::open(config)?,
        })
    }

    /// Inject backends directly. Test-only on purpose: production always builds
    /// the configured backends via `new`/`open`, so a stub can never reach a
    /// real run — the same compiler-enforced boundary the router and classifier
    /// stubs rely on. Un-gate it if a consumer ever needs real injection.
    #[cfg(test)]
    pub(crate) fn from_parts(planner: QueryPlanner, store: VaultStore) -> Self {
        Self { planner, store }
    }

    pub fn planner(&self) -> &QueryPlanner {
        &self.planner
    }

    pub fn store(&self) -> &VaultStore {
        &self.store
    }

    /// Plan and search in one call.
    pub fn retrieve(&self, prompt: &str) -> Result<Retrieval, VaultError> {
        match self.planner.plan(prompt)? {
            None => Ok(Retrieval::Skip(SkipReason::RouterSkip)),
            Some(planned) => self.store.search(&planned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::StubEmbedder;
    use crate::retrieve::{RouterError, StubRouter};
    use crate::store::SqliteStore;

    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    /// The whole reason this is three types rather than one.
    ///
    /// `QueryPlanner` must be `Send + Sync` so a concurrent caller can share one
    /// behind an `Arc` and make router calls with no lock at all. `VaultStore`
    /// only needs `Send` — it owns a rusqlite `Connection`, which is not `Sync`,
    /// so it goes one-per-worker or behind a mutex.
    ///
    /// If a future field made the planner non-`Sync`, every concurrent request
    /// would have to serialise behind a multi-second router timeout. That
    /// regression would be invisible at runtime, so it is pinned here.
    #[test]
    fn the_planner_is_shareable_and_the_store_is_movable() {
        assert_send_sync::<QueryPlanner>();
        assert_send::<VaultStore>();
        assert_send::<Vault>();
    }

    fn stub_vault(config: &Config) -> Vault {
        let store = SqliteStore::open_in_memory(config).expect("store");
        Vault::from_parts(
            QueryPlanner::from_parts(
                Box::new(StubRouter),
                Box::new(StubEmbedder::from_config(config)),
            ),
            VaultStore::from_store(Box::new(store), config.clone()),
        )
    }

    /// `Vault::retrieve` runs both phases: the stub router plans, the stub
    /// embedder vectorises, and the empty store yields nothing.
    #[test]
    fn retrieve_runs_both_phases() {
        let config = Config::default();
        let vault = stub_vault(&config);

        let out = vault
            .retrieve("what does BuildRequest need?")
            .expect("retrieve");

        assert!(
            matches!(out, Retrieval::Skip(SkipReason::NoHits)),
            "empty store should skip, got {out:?}"
        );
    }

    struct SkipRouter;
    impl Router for SkipRouter {
        fn name(&self) -> &'static str {
            "skip-stub"
        }
        fn plan(&self, _prompt: &str) -> Result<RouterOutput, RouterError> {
            Ok(RouterOutput::Skip)
        }
    }

    /// A router skip short-circuits before the embedder runs. This is the
    /// zero-cost passthrough: no embedding call, no store query, no work.
    #[test]
    fn a_router_skip_short_circuits_before_embedding() {
        let config = Config::default();
        let planner = QueryPlanner::from_parts(
            Box::new(SkipRouter),
            Box::new(StubEmbedder::from_config(&config)),
        );

        assert!(planner.route("hi").expect("route").is_none());
        assert!(planner.plan("hi").expect("plan").is_none());
    }

    /// `plan` is the convenience wrapper over the two steps a caller can also
    /// drive separately; the hook uses the separate ones to time them apart.
    #[test]
    fn plan_composes_route_and_embed_query() {
        let config = Config::default();
        let planner = QueryPlanner::from_parts(
            Box::new(StubRouter),
            Box::new(StubEmbedder::from_config(&config)),
        );

        let routed = planner.route("q").expect("route").expect("some plan");
        let embedded = planner.embed_query("q").expect("embed");
        let combined = planner
            .plan("q")
            .expect("plan")
            .expect("some planned query");

        assert_eq!(combined.plan.projects, routed.projects);
        assert_eq!(combined.embedding, embedded);
    }

    #[test]
    fn backend_name_is_reported_for_telemetry() {
        let config = Config::default();
        let planner = QueryPlanner::from_parts(
            Box::new(StubRouter),
            Box::new(StubEmbedder::from_config(&config)),
        );
        assert_eq!(planner.backend(), "stub");
    }
}
