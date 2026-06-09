//! The single home for vault's ranking math.
//!
//! Store backends expose two raw query primitives — `bm25_search` and
//! `cosine_search` — and nothing more. This module blends their result sets into
//! one ranked list. Keeping the blend here (rather than inside each backend)
//! means SQLite, a future Postgres backend, and any test double all share
//! byte-identical scoring, and the blend is unit-testable without a live store.

use std::collections::HashMap;

use crate::store::Hit;

/// How many candidates each arm (BM25, cosine) returns before the merge, and the
/// size of the final ranked list. A retrieval-breadth knob, not a token budget —
/// the token budget trims further downstream in `retrieve::budget`.
pub(crate) const TOP_K: usize = 50;

/// Blend BM25 and cosine result sets into one ranked list.
///
/// `bm25` carries populated `bm25_score`s (its `cosine_score`s are 0); `cosine`
/// carries populated `cosine_score`s (its `bm25_score`s are 0). Both reference
/// the same chunks by `chunk_id`. We union them by id, normalize BM25 against the
/// result-set maximum (cosine is already in `[0, 1]`), blend
/// `final = alpha * bm25_norm + (1 - alpha) * cosine`, sort score-descending, and
/// truncate to `limit`.
///
/// Raw `bm25_score` / `cosine_score` are preserved on each `Hit` for diagnostics;
/// only `final_score` is computed here.
pub fn merge(bm25: Vec<Hit>, cosine: Vec<Hit>, alpha: f32, limit: usize) -> Vec<Hit> {
    let mut by_id: HashMap<i64, Hit> = HashMap::with_capacity(bm25.len() + cosine.len());

    for hit in bm25 {
        by_id.insert(hit.chunk_id, hit);
    }
    for hit in cosine {
        by_id
            .entry(hit.chunk_id)
            // Already seen from the BM25 arm — copy the cosine score onto it.
            .and_modify(|existing| existing.cosine_score = hit.cosine_score)
            // Cosine-only chunk — insert as-is (cosine set, bm25 0).
            .or_insert(hit);
    }

    let mut hits: Vec<Hit> = by_id.into_values().collect();

    let max_bm25 = hits.iter().map(|h| h.bm25_score).fold(0.0_f32, f32::max);
    for h in &mut hits {
        let bm25_norm = if max_bm25 > 0.0 {
            h.bm25_score / max_bm25
        } else {
            0.0
        };
        h.final_score = alpha * bm25_norm + (1.0 - alpha) * h.cosine_score;
    }

    hits.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DocType;

    fn hit(chunk_id: i64, bm25: f32, cosine: f32) -> Hit {
        Hit {
            chunk_id,
            project_id: 1,
            doc_type: DocType::Convention,
            label: format!("chunk-{chunk_id}"),
            content: String::new(),
            token_est: 10,
            bm25_score: bm25,
            cosine_score: cosine,
            final_score: 0.0,
        }
    }

    fn bm25_hit(chunk_id: i64, score: f32) -> Hit {
        hit(chunk_id, score, 0.0)
    }

    fn cosine_hit(chunk_id: i64, score: f32) -> Hit {
        hit(chunk_id, 0.0, score)
    }

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    #[test]
    fn empty_inputs_yield_empty() {
        assert!(merge(vec![], vec![], 0.6, 50).is_empty());
    }

    #[test]
    fn cosine_only_blends_without_bm25() {
        // No BM25 arm (e.g. the router produced no keyword tokens): final score
        // is purely the cosine contribution, bm25_score stays 0.
        let out = merge(vec![], vec![cosine_hit(1, 0.8)], 0.6, 50);
        assert_eq!(out.len(), 1);
        approx(out[0].bm25_score, 0.0);
        approx(out[0].cosine_score, 0.8);
        approx(out[0].final_score, 0.4 * 0.8);
    }

    #[test]
    fn single_bm25_normalizes_to_one() {
        // A lone BM25 hit is the max, so it normalizes to 1.0 regardless of its
        // raw score; final = alpha * 1.0.
        let out = merge(vec![bm25_hit(1, 5.0)], vec![], 0.6, 50);
        approx(out[0].bm25_score, 5.0); // raw preserved
        approx(out[0].final_score, 0.6);
    }

    #[test]
    fn bm25_normalized_against_result_set_max() {
        let out = merge(vec![bm25_hit(1, 5.0), bm25_hit(2, 2.5)], vec![], 0.6, 50);
        // 5.0 → norm 1.0 → final 0.6 ; 2.5 → norm 0.5 → final 0.3
        assert_eq!(out[0].chunk_id, 1);
        approx(out[0].final_score, 0.6);
        assert_eq!(out[1].chunk_id, 2);
        approx(out[1].final_score, 0.3);
    }

    #[test]
    fn overlapping_chunk_combines_both_scores_into_one_entry() {
        let out = merge(vec![bm25_hit(1, 5.0)], vec![cosine_hit(1, 0.9)], 0.6, 50);
        assert_eq!(out.len(), 1, "same chunk_id must collapse to one hit");
        approx(out[0].bm25_score, 5.0);
        approx(out[0].cosine_score, 0.9);
        // norm 1.0 → 0.6*1.0 + 0.4*0.9 = 0.96
        approx(out[0].final_score, 0.96);
    }

    #[test]
    fn sorts_descending_by_final_score() {
        // id=2 wins on cosine despite id=1 having the BM25 hit.
        let out = merge(
            vec![bm25_hit(1, 1.0)],
            vec![cosine_hit(1, 0.0), cosine_hit(2, 1.0)],
            0.2, // weight cosine heavily
            50,
        );
        assert_eq!(out[0].chunk_id, 2);
        assert!(out[0].final_score >= out[1].final_score);
    }

    #[test]
    fn truncates_to_limit() {
        let bm25 = vec![bm25_hit(1, 3.0), bm25_hit(2, 2.0), bm25_hit(3, 1.0)];
        let out = merge(bm25, vec![], 0.6, 2);
        assert_eq!(out.len(), 2);
        // Highest two survive, in order.
        assert_eq!(out[0].chunk_id, 1);
        assert_eq!(out[1].chunk_id, 2);
    }

    #[test]
    fn alpha_one_is_pure_bm25() {
        let out = merge(vec![bm25_hit(1, 4.0)], vec![cosine_hit(1, 0.9)], 1.0, 50);
        // cosine ignored: final = 1.0 * norm(1.0)
        approx(out[0].final_score, 1.0);
    }

    #[test]
    fn alpha_zero_is_pure_cosine() {
        let out = merge(vec![bm25_hit(1, 4.0)], vec![cosine_hit(1, 0.7)], 0.0, 50);
        approx(out[0].final_score, 0.7);
    }

    #[test]
    fn all_zero_bm25_does_not_divide_by_zero() {
        // Degenerate: BM25 arm returned rows but every score is 0. max_bm25 is 0,
        // so bm25_norm must clamp to 0 rather than produce NaN.
        let out = merge(vec![bm25_hit(1, 0.0)], vec![cosine_hit(1, 0.5)], 0.6, 50);
        approx(out[0].final_score, 0.4 * 0.5);
        assert!(out[0].final_score.is_finite());
    }
}
