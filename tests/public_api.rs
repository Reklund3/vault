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
    Context, DocType, EmbedError, Hit, Language, PlannedQuery, QueryPlan, Retrieval, RouterError,
    SkipReason, StoreError, VaultError,
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
