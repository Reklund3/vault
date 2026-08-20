//! Smoke test for the public surface, compiled as a separate crate.
//!
//! This is the only thing that actually exercises the lib/CLI split. The unit
//! tests live *inside* the crate, so they can reach `pub(crate)` items and
//! prove nothing about what a consumer can see. A service — or a future MCP
//! server — sees exactly what this file sees.
//!
//! Deliberately narrow: it checks that the types a consumer needs are nameable
//! and usable, not that retrieval works. The full integration suite is a later
//! step; this exists so an accidental privatisation fails the build.

// NOTE the shape of these paths. `Config` lives behind `vault::config` while
// the retrieval types are re-exported at the root — an inconsistency the
// curation step is meant to settle. Written as-is so this file reflects the
// surface a consumer actually sees today, not the one we intend.
use vault::config::Config;
use vault::{
    Context, DocType, EmbedError, Hit, Interaction, Language, PlannedQuery, QueryPlan,
    QueryPlanner, Retrieval, RouterError, SkipReason, StoreError, SyncError, SyncOptions,
    SyncReport, Vault, VaultError, VaultStore,
};

#[test]
fn config_is_constructible_from_outside_the_crate() {
    // `Config::load` reads ~/.vault and would couple this test to the machine,
    // but the type and its accessors must be reachable.
    let config = Config::default();
    assert!(config.embedding_dim() > 0);
    assert!(!config.default_context_tag().is_empty());
}

#[test]
fn error_variants_are_matchable_by_a_consumer() {
    // The point of VaultError: a caller distinguishes failures without parsing
    // strings. If these variants were not public, a consumer could only match
    // on the Display text.
    let err = VaultError::RouterBuild(RouterError::MissingApiKey {
        env_var: "ANTHROPIC_API_KEY".into(),
    });
    assert!(matches!(err, VaultError::RouterBuild(_)));

    let err = VaultError::Query(StoreError::Backend("boom".into()));
    assert!(matches!(err, VaultError::Query(_)));

    let err = VaultError::EmbedQuery(EmbedError::Transport("down".into()));
    assert!(matches!(err, VaultError::EmbedQuery(_)));
}

#[test]
fn the_underlying_error_is_reachable_through_source() {
    use std::error::Error;

    let err = VaultError::DbOpen(StoreError::Backend("disk gone".into()));
    let source = err.source().expect("source must be attached");

    assert!(source.to_string().contains("disk gone"));
}

#[test]
fn skip_reasons_are_visible_so_a_caller_can_tell_them_apart() {
    // "The router judged this self-contained" and "the store had nothing"
    // call for different follow-up, so a consumer must be able to distinguish
    // them rather than seeing one empty result.
    assert_eq!(SkipReason::RouterSkip.as_str(), "router-skip");
    assert_eq!(SkipReason::NoHits.as_str(), "no-hits");
    assert_ne!(SkipReason::RouterSkip, SkipReason::NoHits);
}

#[test]
fn context_can_be_built_and_rendered_by_a_consumer() {
    // A consumer that wants vault's framing calls render_block; one that wants
    // the chunks reads `hits`. Both must be reachable from outside.
    let context = Context {
        tag: "vault-context".to_string(),
        hits: Vec::new(),
        tokens: 0,
    };

    assert_eq!(
        context.render_block(),
        "<vault-context>\n</vault-context>\n"
    );
    assert!(context.hits.is_empty());
}

/// Regression guard for half-public types.
///
/// `Context.hits` and `PlannedQuery.plan` are public fields, so every type
/// reachable through them must be nameable from out here. It is not enough that
/// the field can be *read*: a consumer needs to write signatures against these
/// types, and `Hit` was briefly unnameable while `Context.hits` was public.
#[test]
fn every_type_reachable_through_a_public_field_is_nameable() {
    fn takes_a_hit(hit: &Hit) -> &str {
        &hit.label
    }
    fn takes_a_plan(plan: &QueryPlan) -> usize {
        plan.projects.len()
    }

    let hit = Hit {
        chunk_id: 1,
        project_id: 1,
        doc_type: DocType::Contract,
        label: "BuildRequest".to_string(),
        content: "message BuildRequest {}".to_string(),
        token_est: 4,
        bm25_score: 0.5,
        cosine_score: 0.5,
        final_score: 0.5,
    };
    assert_eq!(takes_a_hit(&hit), "BuildRequest");

    let planned = PlannedQuery {
        plan: QueryPlan {
            projects: vec!["vault".to_string()],
            type_names: vec![],
            topics: vec![],
            doc_types: vec![DocType::Contract],
            languages: vec![Language::Proto],
        },
        embedding: vec![0.0; 8],
    };
    assert_eq!(takes_a_plan(&planned.plan), 1);
}

/// A consumer branches on the outcome without parsing strings — the shape the
/// hook's pre-rendered block could not offer.
#[test]
fn retrieval_can_be_matched_by_a_consumer() {
    let skipped = Retrieval::Skip(SkipReason::RouterSkip);
    let described = match &skipped {
        Retrieval::Skip(reason) => reason.as_str(),
        Retrieval::Context(_) => "context",
    };
    assert_eq!(described, "router-skip");

    let with_context = Retrieval::Context(Context {
        tag: "software-context".to_string(),
        hits: Vec::new(),
        tokens: 0,
    });
    assert!(matches!(with_context, Retrieval::Context(_)));
}

/// The concurrency contract, verified from outside the crate.
///
/// A consumer building a service or an MCP server needs to know what it may
/// share and what it must not. `QueryPlanner` is `Send + Sync`, so one instance
/// goes behind an `Arc` and serves every request with no lock. `VaultStore` owns
/// a SQLite connection and is `Send` only, so it is one-per-worker or behind a
/// mutex — and because only that half needs the lock, a router call never
/// blocks another request.
#[test]
fn the_concurrency_contract_holds_for_consumers() {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<QueryPlanner>();
    assert_send::<VaultStore>();
    assert_send::<Vault>();
}

/// A consumer that is not a terminal must be able to say so, and must be able
/// to build a `SyncOptions` without one.
///
/// The absence of a `Default` on `SyncOptions` is the point: `Interaction` has
/// no safe guess. Defaulting to `Terminal` would leave a service blocked on a
/// read of a stdin that belongs to somebody else's protocol, and defaulting to
/// `NonInteractive` would decide a billing question on the caller's behalf.
#[test]
fn a_non_terminal_consumer_can_configure_a_sync() {
    let opts = SyncOptions {
        repo: std::path::PathBuf::from("/tmp/some-repo"),
        explicit_name: Some("vault".to_string()),
        explicit_domain: Some("software".to_string()),
        dry_run: true,
        interaction: Interaction::NonInteractive {
            allow_remote_billing: false,
        },
    };

    assert_eq!(
        opts.interaction,
        Interaction::NonInteractive {
            allow_remote_billing: false
        }
    );
    assert_ne!(opts.interaction, Interaction::Terminal);
}

/// The consent refusal has to be distinguishable from a user declining, or a
/// caller cannot tell "retry with consent" from "the user said no".
#[test]
fn a_consumer_can_tell_a_missing_consent_from_a_refused_one() {
    let err = VaultError::Sync(SyncError::RemoteBillingNotPermitted { backend: "haiku" });

    match &err {
        VaultError::Sync(SyncError::RemoteBillingNotPermitted { backend }) => {
            assert_eq!(*backend, "haiku");
        }
        other => panic!("wrong variant: {other}"),
    }

    let declined = VaultError::Sync(SyncError::DeclinedRemoteCost);
    assert_ne!(err.to_string(), declined.to_string());
}

/// `SyncReport` is the return value of `Vault::sync`, so its fields have to be
/// readable from out here — a consumer reporting on an unattended index run has
/// nothing else to go on.
#[test]
fn a_consumer_can_read_a_sync_report() {
    let report = SyncReport {
        project: "vault".to_string(),
        files_walked: 12,
        chunks_indexed: 40,
        ..SyncReport::default()
    };

    assert_eq!(report.project, "vault");
    assert_eq!(report.files_walked, 12);
    assert_eq!(report.chunks_indexed, 40);
    assert_eq!(report.domain, None);
}
