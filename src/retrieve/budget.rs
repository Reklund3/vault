use crate::store::Hit;

/// The result of fitting ranked hits into a token budget. `tokens_used` is the
/// running sum of `token_est` over the selected chunks — the hook records it
/// in the `~/.vault/hook.log` telemetry record for each injection.
#[derive(Debug, Clone)]
pub struct BudgetedSelection {
    pub chunks: Vec<Hit>,
    pub tokens_used: u32,
}

/// Pick the highest-scoring hits whose `token_est` sum stays within
/// `token_budget`, dropping any below `min_score`. **`continue` past oversized
/// chunks** rather than `break` — a smaller later chunk may still fit the
/// remaining budget. Input order (score-descending) is preserved in output.
///
/// `max_hits` caps how many chunks come back; `None` is uncapped. It counts
/// *selected* chunks, not candidates examined, so a cap of 4 yields the four
/// highest-scoring chunks that actually fit — an oversized chunk skipped by the
/// budget check does not consume one of the four. This is the only one of the
/// three limits that can stop the loop early: `min_score` and the budget both
/// have to keep looking for a later chunk that qualifies, but once the cap is
/// full nothing further can qualify.
pub fn select_within_budget(
    hits: Vec<Hit>,
    token_budget: u32,
    min_score: f32,
    max_hits: Option<usize>,
) -> BudgetedSelection {
    // A cap of zero selects nothing; the loop below would otherwise treat it
    // the same as uncapped on its first iteration.
    if max_hits == Some(0) {
        return BudgetedSelection {
            chunks: Vec::new(),
            tokens_used: 0,
        };
    }

    let mut chunks = Vec::new();
    let mut tokens_used: u32 = 0;
    for hit in hits {
        // `>=`, not `!(< min)`: every comparison with NaN is false, so a
        // `final_score < min_score` test *admits* NaN rather than dropping it.
        // NaN is reachable — `merge` divides by the result-set max BM25, and a
        // zero-norm embedding makes cosine `0/0` — and a NaN score then sorts
        // unpredictably and injects a junk chunk that no threshold can stop.
        if !matches!(
            hit.final_score.partial_cmp(&min_score),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ) {
            continue;
        }
        if tokens_used.saturating_add(hit.token_est) > token_budget {
            continue;
        }
        // Saturating, to match the guard directly above. The guard lets a hit
        // through when the saturated sum is not *greater* than the budget, so a
        // budget at `u32::MAX` admits a hit that then overflows a plain `+=` —
        // a debug-build panic on the hook's hot path. Unreachable while
        // `token_budget` is a `u16` in config, which is exactly why the two
        // lines are worth keeping in agreement: the day the budget widens, the
        // bug would arrive with no code change here.
        tokens_used = tokens_used.saturating_add(hit.token_est);
        chunks.push(hit);
        if max_hits.is_some_and(|cap| chunks.len() >= cap) {
            break;
        }
    }
    BudgetedSelection {
        chunks,
        tokens_used,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DocType;

    fn hit(id: i64, score: f32, tokens: u32) -> Hit {
        Hit {
            chunk_id: id,
            project_id: 1,
            doc_type: DocType::Convention,
            label: format!("hit-{id}"),
            content: String::new(),
            token_est: tokens,
            bm25_score: 0.0,
            cosine_score: 0.0,
            final_score: score,
        }
    }

    #[test]
    fn empty_input_yields_empty_selection() {
        let sel = select_within_budget(vec![], 10_000, 0.15, None);
        assert!(sel.chunks.is_empty());
        assert_eq!(sel.tokens_used, 0);
    }

    #[test]
    fn all_hits_fit_when_budget_is_ample() {
        let hits = vec![hit(1, 0.9, 100), hit(2, 0.8, 200), hit(3, 0.7, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(sel.tokens_used, 350);
    }

    #[test]
    fn min_score_gate_drops_below_threshold() {
        let hits = vec![hit(1, 0.9, 50), hit(2, 0.10, 50), hit(3, 0.5, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(sel.tokens_used, 100);
    }

    #[test]
    fn min_score_gate_drops_nan_score() {
        let hits = vec![hit(1, f32::NAN, 50), hit(2, 0.9, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![2],
            "NaN score must be dropped by min_score gate"
        );
    }

    #[test]
    fn min_score_at_exactly_threshold_is_included() {
        let hits = vec![hit(1, 0.15, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, None);
        assert_eq!(sel.chunks.len(), 1);
    }

    #[test]
    fn oversized_chunk_skipped_but_smaller_later_chunk_packs_in() {
        // Top-scored hit is too big for the budget — we must `continue`, not
        // `break`. Lower-scored but smaller hits should still fit the gap.
        let hits = vec![hit(1, 0.9, 9000), hit(2, 0.8, 100), hit(3, 0.7, 50)];
        let sel = select_within_budget(hits, 200, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(sel.tokens_used, 150);
    }

    #[test]
    fn exact_budget_boundary_is_inclusive() {
        // 100 + 100 = 200 == budget; both fit. 100 + 101 would not.
        let hits = vec![hit(1, 0.9, 100), hit(2, 0.8, 100)];
        let sel = select_within_budget(hits, 200, 0.15, None);
        assert_eq!(sel.chunks.len(), 2);
        assert_eq!(sel.tokens_used, 200);
    }

    #[test]
    fn one_token_over_budget_is_excluded() {
        let hits = vec![hit(1, 0.9, 100), hit(2, 0.8, 101)];
        let sel = select_within_budget(hits, 200, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(sel.tokens_used, 100);
    }

    #[test]
    fn zero_budget_selects_nothing() {
        let hits = vec![hit(1, 0.9, 1)];
        let sel = select_within_budget(hits, 0, 0.0, None);
        assert!(sel.chunks.is_empty());
        assert_eq!(sel.tokens_used, 0);
    }

    #[test]
    fn zero_min_score_disables_the_gate() {
        let hits = vec![hit(1, 0.0, 10), hit(2, 0.001, 10)];
        let sel = select_within_budget(hits, 100, 0.0, None);
        assert_eq!(sel.chunks.len(), 2);
    }

    #[test]
    fn input_order_is_preserved_in_output() {
        // Caller is responsible for score-descending order; we don't re-sort.
        // Pass deliberately out-of-order input; output must follow input.
        let hits = vec![hit(1, 0.5, 10), hit(2, 0.9, 10), hit(3, 0.7, 10)];
        let sel = select_within_budget(hits, 100, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    /// Covers the *guard* only: with a budget of 100 the huge hit is rejected
    /// before the accumulator ever sees it, so this passed throughout the window
    /// when `tokens_used += ...` could overflow. The accumulator is covered by
    /// `a_budget_at_the_type_ceiling_saturates_instead_of_panicking` below.
    #[test]
    fn saturating_add_does_not_panic_on_overflow() {
        // token_est of u32::MAX shouldn't crash; it just fails the budget check.
        let hits = vec![hit(1, 0.9, u32::MAX), hit(2, 0.8, 50)];
        let sel = select_within_budget(hits, 100, 0.15, None);
        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(sel.tokens_used, 50);
    }

    // ----- max_hits cap -----

    /// The cap keeps the highest-scoring chunks, since input arrives
    /// score-descending and the loop preserves that order.
    #[test]
    fn max_hits_keeps_only_the_highest_scoring_chunks() {
        let hits = vec![
            hit(1, 0.9, 50),
            hit(2, 0.8, 50),
            hit(3, 0.7, 50),
            hit(4, 0.6, 50),
            hit(5, 0.5, 50),
        ];
        let sel = select_within_budget(hits, 10_000, 0.15, Some(3));

        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(sel.tokens_used, 150, "dropped chunks must not be counted");
    }

    #[test]
    fn max_hits_above_the_available_count_changes_nothing() {
        let hits = vec![hit(1, 0.9, 50), hit(2, 0.8, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, Some(10));

        assert_eq!(sel.chunks.len(), 2);
        assert_eq!(sel.tokens_used, 100);
    }

    // ----- accumulator overflow -----

    /// The budget guard saturates; the accumulator has to as well.
    ///
    /// `saturating_add(x) > budget` is false when the sum saturates at exactly
    /// `u32::MAX` and the budget *is* `u32::MAX`, so the hit is admitted — and a
    /// plain `tokens_used += ...` then panics in a debug build. Nothing reaches
    /// this today: `Config::token_budget` is a `u16`, so the widest real budget
    /// is 65_535. The test exists because the function is `pub` and takes a
    /// `u32`, which is the contract a future caller will read.
    #[test]
    fn a_budget_at_the_type_ceiling_saturates_instead_of_panicking() {
        let hits = vec![hit(1, 0.9, u32::MAX), hit(2, 0.8, 10)];
        let sel = select_within_budget(hits, u32::MAX, 0.0, None);

        assert_eq!(sel.chunks.len(), 2, "both hits fit a u32::MAX budget");
        assert_eq!(
            sel.tokens_used,
            u32::MAX,
            "the sum saturates at the ceiling rather than wrapping"
        );
    }

    /// The cap counts *selected* chunks, not candidates examined. An oversized
    /// chunk that the budget skips must not consume one of the slots, or a
    /// single fat chunk near the top would silently shrink the result.
    #[test]
    fn an_oversized_chunk_does_not_consume_a_cap_slot() {
        let hits = vec![
            hit(1, 0.9, 50),
            hit(2, 0.85, 9_000), // over the budget below — skipped, not counted
            hit(3, 0.8, 50),
        ];
        let sel = select_within_budget(hits, 200, 0.15, Some(2));

        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    /// Same for a below-threshold chunk: `min_score` filters it out before the
    /// cap ever sees it.
    #[test]
    fn a_below_threshold_chunk_does_not_consume_a_cap_slot() {
        let hits = vec![hit(1, 0.9, 50), hit(2, 0.10, 50), hit(3, 0.8, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, Some(2));

        assert_eq!(
            sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn no_cap_selects_everything_that_fits() {
        let hits = vec![hit(1, 0.9, 50), hit(2, 0.8, 50), hit(3, 0.7, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, None);

        assert_eq!(sel.chunks.len(), 3);
    }

    /// A cap of zero is a real answer, not a synonym for uncapped. Guarding it
    /// explicitly because the `chunks.len() >= cap` check only runs *after* a
    /// push, so without the early return the first chunk would slip through.
    #[test]
    fn a_zero_cap_selects_nothing() {
        let hits = vec![hit(1, 0.9, 50), hit(2, 0.8, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15, Some(0));

        assert!(sel.chunks.is_empty());
        assert_eq!(sel.tokens_used, 0);
    }
}
