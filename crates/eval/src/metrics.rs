//! Standard IR metrics.
//!
//! These are the vocabulary of relevance work. If you can't put a number on
//! a ranking change, you're not tuning — you're guessing, and you will
//! reliably make things worse while believing you improved them.

/// Discounted Cumulative Gain at k.
///
/// Rewards putting highly-relevant documents *early*: the gain from a
/// document is discounted by log2 of its rank, so position 1 counts far
/// more than position 10. This is the metric that matches how people
/// actually consume a result page.
pub fn dcg_at_k(gains: &[f32], k: usize) -> f32 {
    gains
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &g)| g / ((i + 2) as f32).log2())
        .sum()
}

/// NDCG@k — DCG normalised by the best achievable DCG for this query, so
/// scores are comparable across queries with different numbers of relevant
/// documents. 1.0 means a perfect ranking.
pub fn ndcg_at_k(gains: &[f32], ideal: &[f32], k: usize) -> f32 {
    let ideal_dcg = dcg_at_k(ideal, k);
    if ideal_dcg <= 0.0 {
        return 0.0;
    }
    dcg_at_k(gains, k) / ideal_dcg
}

/// Reciprocal rank of the first relevant result (0 if none in the list).
/// Averaged over queries this is MRR — the right metric when there's
/// essentially one correct answer, as with navigational queries.
pub fn reciprocal_rank(gains: &[f32], relevant_threshold: f32) -> f32 {
    gains
        .iter()
        .position(|&g| g >= relevant_threshold)
        .map(|i| 1.0 / (i + 1) as f32)
        .unwrap_or(0.0)
}

pub fn precision_at_k(gains: &[f32], k: usize, relevant_threshold: f32) -> f32 {
    if k == 0 {
        return 0.0;
    }
    let hits = gains.iter().take(k).filter(|&&g| g >= relevant_threshold).count();
    hits as f32 / k.min(gains.len().max(1)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_ranking_scores_one() {
        let gains = vec![3.0, 2.0, 1.0];
        let ideal = vec![3.0, 2.0, 1.0];
        assert!((ndcg_at_k(&gains, &ideal, 3) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn reversed_ranking_scores_below_perfect() {
        let ideal = vec![3.0, 2.0, 1.0];
        let reversed = vec![1.0, 2.0, 3.0];
        assert!(ndcg_at_k(&reversed, &ideal, 3) < 1.0);
    }

    #[test]
    fn position_matters_more_than_presence() {
        let ideal = vec![3.0, 0.0, 0.0];
        let first = ndcg_at_k(&[3.0, 0.0, 0.0], &ideal, 3);
        let last = ndcg_at_k(&[0.0, 0.0, 3.0], &ideal, 3);
        assert!(first > last, "rank 1 ({first}) should beat rank 3 ({last})");
    }

    #[test]
    fn reciprocal_rank_finds_first_hit() {
        assert_eq!(reciprocal_rank(&[0.0, 0.0, 2.0], 1.0), 1.0 / 3.0);
        assert_eq!(reciprocal_rank(&[2.0], 1.0), 1.0);
        assert_eq!(reciprocal_rank(&[0.0, 0.0], 1.0), 0.0);
    }

    #[test]
    fn no_relevant_docs_yields_zero_not_nan() {
        assert_eq!(ndcg_at_k(&[0.0, 0.0], &[0.0, 0.0], 2), 0.0);
    }
}
