//! Black-box behaviour tests against the public API.
//!
//! `public_api.rs` checks that the types a consumer needs are *nameable*. This
//! file checks that the pipeline *works* when driven from outside the crate,
//! with no access to `pub(crate)` internals and no stubs — `StubRouter` and
//! `StubClassifier` are `#[cfg(test)]`-gated, which means they do not exist
//! here. That is the point: everything below runs against the real types a
//! service or an MCP server would use.
//!
//! Nothing here needs the network. `VaultStore::search` takes a `PlannedQuery`,
//! whose fields are public, so a consumer can construct one directly instead of
//! going through a router and an embedder. That separation is design rule 4, and
//! this file is where it pays off: the store half is testable on its own.

mod common;

use common::{TmpDir, config_in, plan_for};
use vault::{Interaction, QueryPlanner, Retrieval, SkipReason, SyncOptions, VaultStore};

/// Opening a store creates and migrates the database, in the directory the
/// caller named rather than `~/.vault`.
#[test]
fn opening_a_store_creates_the_database_where_the_consumer_asked() {
    let tmp = TmpDir::new("creates");
    let config = config_in(tmp.path());

    let db_path = config.db_path().expect("db path");
    assert!(!db_path.exists(), "precondition: nothing there yet");

    let _store = VaultStore::open(&config).expect("open");

    assert!(db_path.exists(), "open must create the database");
    assert_eq!(db_path, tmp.path().join("vault.db"));
}

/// An empty store is not an error. A consumer distinguishing "nothing matched"
/// from "retrieval failed" is the whole reason `Retrieval` is an enum rather
/// than an empty `Vec`.
#[test]
fn searching_an_empty_store_skips_rather_than_failing() {
    let tmp = TmpDir::new("empty");
    let config = config_in(tmp.path());
    let store = VaultStore::open(&config).expect("open");

    let out = store
        .search(&plan_for(&config, vec!["vault".to_string()]))
        .expect("search must succeed on an empty store");

    match out {
        Retrieval::Skip(SkipReason::NoHits) => {}
        other => panic!("expected a no-hits skip, got {other:?}"),
    }
}

/// The payoff of splitting `QueryPlanner` from `VaultStore`: the store half runs
/// with no router, no embedder, and no network. If `search` ever grew a
/// dependency on either, this test would need one too — and that is exactly the
/// regression worth catching, because it would put a multi-second network
/// timeout inside the lock a concurrent consumer holds.
#[test]
fn the_store_half_runs_with_no_network_at_all() {
    let tmp = TmpDir::new("nonetwork");
    let config = config_in(tmp.path());

    // No QueryPlanner is constructed anywhere in this test. Building one would
    // resolve a router backend and could require an API key; the store does not.
    let store = VaultStore::open(&config).expect("open");
    let planned = plan_for(&config, vec![]);

    assert!(store.search(&planned).is_ok());
}

/// A second connection to the same database succeeds while the first is open.
///
/// This is Step 2's WAL pragma verified from outside the crate. Under the
/// default rollback journal a reader's SHARED lock can refuse a writer's commit,
/// which is what blocks "one `VaultStore` per worker".
#[test]
fn two_stores_can_hold_the_same_database_at_once() {
    let tmp = TmpDir::new("concurrent");
    let config = config_in(tmp.path());

    let first = VaultStore::open(&config).expect("first open");
    let second = VaultStore::open(&config).expect("second open must not be refused");

    let planned = plan_for(&config, vec![]);
    assert!(first.search(&planned).is_ok());
    assert!(second.search(&planned).is_ok());
}

/// A dry-run sync driven by a consumer that has no terminal.
///
/// This is the unattended path end to end: it walks a real directory, returns a
/// real `SyncReport`, and never reads stdin. It short-circuits before TEI and
/// the classifier, so it runs with no services up — which is what makes it
/// suitable for CI.
#[test]
fn a_dry_run_sync_completes_unattended() {
    let vault_dir = TmpDir::new("sync-vault");
    let repo = TmpDir::new("sync-repo");
    repo.write("api.proto", "syntax = \"proto3\";\nmessage Build {}\n");
    repo.write("docs/design.md", "# Design\n\n## Overview\n\ntext\n");

    let config = config_in(vault_dir.path());
    let mut store = VaultStore::open(&config).expect("open");

    let report = store
        .sync(SyncOptions {
            repo: repo.path().to_path_buf(),
            explicit_name: Some("fixture".to_string()),
            explicit_domain: None,
            dry_run: true,
            interaction: Interaction::NonInteractive {
                allow_remote_billing: false,
            },
        })
        .expect("a dry run needs no services and no consent");

    assert!(report.dry_run);
    assert_eq!(report.project, "fixture");
    assert_eq!(report.files_walked, 2);
    assert_eq!(report.files_would_classify, 2);
    // Dry run touches no store and calls nothing remote.
    assert_eq!(report.chunks_indexed, 0);
    assert_eq!(report.files_classified, 0);
}

/// A non-interactive sync with no `--name` takes the directory-derived name
/// instead of prompting for one. If the interaction policy regressed, this test
/// would block forever rather than fail — which is worth knowing about.
#[test]
fn a_non_interactive_sync_derives_a_name_instead_of_prompting() {
    let vault_dir = TmpDir::new("derive-vault");
    let repo = TmpDir::new("derive-repo");
    repo.write("a.proto", "syntax = \"proto3\";\n");

    let config = config_in(vault_dir.path());
    let mut store = VaultStore::open(&config).expect("open");

    let report = store
        .sync(SyncOptions {
            repo: repo.path().to_path_buf(),
            explicit_name: None,
            explicit_domain: None,
            dry_run: true,
            interaction: Interaction::NonInteractive {
                allow_remote_billing: false,
            },
        })
        .expect("must not prompt");

    let derived = repo
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .expect("dir name");
    assert_eq!(report.project, derived);
}

/// A consumer gets `EmptyPrompt` — not a silent empty result, and not a
/// billable round trip.
///
/// This variant was public but unreachable: the guard lived only in the CLI
/// hook, so `Vault::retrieve("")` called the router, embedded the empty string,
/// and queried SQLite before returning nothing. The deterministic coverage is
/// the inline test in `src/vault.rs`, which can use stubs; this one proves it
/// from outside the crate wherever a backend happens to be configured.
#[test]
fn a_consumer_gets_empty_prompt_rather_than_a_billable_round_trip() {
    let tmp = TmpDir::new("empty-prompt");
    let config = config_in(tmp.path());

    let Ok(vault) = vault::Vault::open(&config) else {
        eprintln!("skipped: no router backend configured on this machine");
        return;
    };

    for prompt in ["", "   ", "\n\t "] {
        match vault.retrieve(prompt).expect("must not error") {
            Retrieval::Skip(SkipReason::EmptyPrompt) => {}
            other => panic!("expected EmptyPrompt for {prompt:?}, got {other:?}"),
        }
    }
}

/// The router step short-circuits on a blank prompt, so no HTTP request is made.
///
/// Driven through `QueryPlanner` rather than `Vault` because this is the seam
/// that used to pay for the call. Constructing a planner needs a configured
/// backend, so the assertion is on `route` returning `None` before it would
/// reach one — an unreachable backend would surface as `Err`, not `Ok(None)`.
#[test]
fn a_blank_prompt_never_reaches_the_router() {
    let tmp = TmpDir::new("blank-route");
    let config = config_in(tmp.path());

    // `QueryPlanner::new` resolves a backend, which may legitimately fail on a
    // machine with no Gemma and no API key — that is not what is under test.
    let Ok(planner) = QueryPlanner::new(&config) else {
        eprintln!("skipped: no router backend available on this machine");
        return;
    };

    for prompt in ["", "   ", "\n\t "] {
        assert!(
            planner.route(prompt).expect("must not error").is_none(),
            "blank prompt {prompt:?} must short-circuit before the router"
        );
    }
}
