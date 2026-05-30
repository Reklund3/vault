use crate::store::Hit;

/// The result of fitting ranked hits into a token budget. `tokens_used` is the
/// running sum of `token_est` over the selected chunks — the hook hands it to
/// `RetrievalLogEntry.tokens_injected` and the context-block builder uses it
/// for telemetry.
#[derive(Debug, Clone)]
pub struct BudgetedSelection {
    pub chunks: Vec<Hit>,
    pub tokens_used: u32,
}

/// Pick the highest-scoring hits whose `token_est` sum stays within
/// `token_budget`, dropping any below `min_score`. **`continue` past oversized
/// chunks** rather than `break` — a smaller later chunk may still fit the
/// remaining budget. Input order (score-descending) is preserved in output.
pub fn select_within_budget(
    hits: Vec<Hit>,
    token_budget: u32,
    min_score: f32,
) -> BudgetedSelection {
    let mut chunks = Vec::new();
    let mut tokens_used: u32 = 0;
    for hit in hits {
        if hit.final_score < min_score {
            continue;
        }
        if tokens_used.saturating_add(hit.token_est) > token_budget {
            continue;
        }
        tokens_used += hit.token_est;
        chunks.push(hit);
    }
    BudgetedSelection { chunks, tokens_used }
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
        let sel = select_within_budget(vec![], 10_000, 0.15);
        assert!(sel.chunks.is_empty());
        assert_eq!(sel.tokens_used, 0);
    }

    #[test]
    fn all_hits_fit_when_budget_is_ample() {
        let hits = vec![hit(1, 0.9, 100), hit(2, 0.8, 200), hit(3, 0.7, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15);
        assert_eq!(sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(sel.tokens_used, 350);
    }

    #[test]
    fn min_score_gate_drops_below_threshold() {
        let hits = vec![hit(1, 0.9, 50), hit(2, 0.10, 50), hit(3, 0.5, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15);
        assert_eq!(sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(sel.tokens_used, 100);
    }

    #[test]
    fn min_score_at_exactly_threshold_is_included() {
        let hits = vec![hit(1, 0.15, 50)];
        let sel = select_within_budget(hits, 10_000, 0.15);
        assert_eq!(sel.chunks.len(), 1);
    }

    #[test]
    fn oversized_chunk_skipped_but_smaller_later_chunk_packs_in() {
        // Top-scored hit is too big for the budget — we must `continue`, not
        // `break`. Lower-scored but smaller hits should still fit the gap.
        let hits = vec![hit(1, 0.9, 9000), hit(2, 0.8, 100), hit(3, 0.7, 50)];
        let sel = select_within_budget(hits, 200, 0.15);
        assert_eq!(sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(sel.tokens_used, 150);
    }

    #[test]
    fn exact_budget_boundary_is_inclusive() {
        // 100 + 100 = 200 == budget; both fit. 100 + 101 would not.
        let hits = vec![hit(1, 0.9, 100), hit(2, 0.8, 100)];
        let sel = select_within_budget(hits, 200, 0.15);
        assert_eq!(sel.chunks.len(), 2);
        assert_eq!(sel.tokens_used, 200);
    }

    #[test]
    fn one_token_over_budget_is_excluded() {
        let hits = vec![hit(1, 0.9, 100), hit(2, 0.8, 101)];
        let sel = select_within_budget(hits, 200, 0.15);
        assert_eq!(sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(sel.tokens_used, 100);
    }

    #[test]
    fn zero_budget_selects_nothing() {
        let hits = vec![hit(1, 0.9, 1)];
        let sel = select_within_budget(hits, 0, 0.0);
        assert!(sel.chunks.is_empty());
        assert_eq!(sel.tokens_used, 0);
    }

    #[test]
    fn zero_min_score_disables_the_gate() {
        let hits = vec![hit(1, 0.0, 10), hit(2, 0.001, 10)];
        let sel = select_within_budget(hits, 100, 0.0);
        assert_eq!(sel.chunks.len(), 2);
    }

    #[test]
    fn input_order_is_preserved_in_output() {
        // Caller is responsible for score-descending order; we don't re-sort.
        // Pass deliberately out-of-order input; output must follow input.
        let hits = vec![hit(1, 0.5, 10), hit(2, 0.9, 10), hit(3, 0.7, 10)];
        let sel = select_within_budget(hits, 100, 0.15);
        assert_eq!(sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn saturating_add_does_not_panic_on_overflow() {
        // token_est of u32::MAX shouldn't crash; it just fails the budget check.
        let hits = vec![hit(1, 0.9, u32::MAX), hit(2, 0.8, 50)];
        let sel = select_within_budget(hits, 100, 0.15);
        assert_eq!(sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(sel.tokens_used, 50);
    }
}
