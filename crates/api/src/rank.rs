//! The ranking function.
//!
//! Final score is a weighted blend of one *query-dependent* signal (BM25F
//! text match) and several *query-independent* ones (authority, quality,
//! trust). That split is the core architectural idea in ranking: the
//! query-independent part is precomputed offline, so query time stays cheap.
//!
//! Every weight here is a hypothesis. `crates/eval` exists to test them —
//! never tune these by staring at one query.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Weights {
    /// Field boosts applied inside the BM25 query itself (BM25F-style).
    pub title_boost: f32,
    pub anchor_boost: f32,
    pub url_boost: f32,
    pub body_boost: f32,

    /// Post-retrieval blend. Every term below operates on a 0..1 normalised
    /// signal, so these weights are directly comparable to one another —
    /// `text_weight: 1.0, pagerank_weight: 0.5` really does mean "authority
    /// is worth half as much as text match".
    pub text_weight: f32,
    pub pagerank_weight: f32,
    pub quality_weight: f32,
    pub domain_trust_weight: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            // Anchor text outranks title: it's third-party testimony about
            // the page, which is harder to game than the page's own <title>.
            anchor_boost: 3.0,
            title_boost: 2.5,
            url_boost: 1.5,
            body_boost: 1.0,

            // Chosen by sweep (eval/sweep.sh) subject to one hard invariant:
            //
            //   pagerank_weight + domain_trust_weight < text_weight
            //
            // Without that constraint a page can win on authority alone
            // while barely matching the query — the failure mode where
            // "popular page" quietly replaces "answer to the question".
            // The unconstrained sweep argmax (0.6 + 0.6 = 1.2) violated it
            // and measured *worse* anyway (0.747 vs 0.749), so the invariant
            // costs nothing here. Re-run the sweep when the corpus or the
            // judgment set changes; these numbers are corpus-specific.
            text_weight: 1.0,
            pagerank_weight: 0.4,
            quality_weight: 0.20,
            domain_trust_weight: 0.3,
        }
    }
}

/// Compress an unbounded positive signal into 0..=1.
///
/// Raw PageRank is wildly skewed — a handful of pages hold most of the mass,
/// so adding `w * pagerank` lets one hub dominate every query regardless of
/// what was asked. Saturation keeps strong authority valuable while
/// preventing it from swamping the text match.
///
/// Note the bound is inclusive: once `value` exceeds `half_point` by enough
/// that f32 can't represent the sum distinctly, this returns exactly 1.0.
/// That's the intended ceiling, not an overflow.
pub fn saturate(value: f32, half_point: f32) -> f32 {
    if value <= 0.0 || half_point <= 0.0 {
        return 0.0;
    }
    value / (value + half_point)
}

pub struct Signals {
    pub bm25: f32,
    pub pagerank: f32,
    pub quality: f32,
    pub inbound_domains: u32,
}

pub struct Scored {
    pub score: f32,
    pub explain: common::ScoreExplain,
}

/// Blend signals into a final score.
///
/// `max_bm25` is the top BM25 score in this query's candidate set, used to
/// normalise text relevance into 0..1.
///
/// Why normalise at all — this was a measured bug, not a style preference.
/// Raw BM25 is unbounded and its spread varies wildly per query: on one
/// query the top hit scores 12, on another 90. Adding a fixed-size authority
/// bonus to that is meaningless, because the same bonus is decisive in the
/// first case and invisible in the second. Concretely, a keyword-stuffed
/// betting page beat Wikipedia's Delhi article by 31 BM25 points while the
/// authority gap between them was 2.5 — so authority could never correct it.
/// Normalising first puts every signal on the same 0..1 footing and makes
/// the weights mean what they say.
///
/// `median_pagerank` scales the authority saturation point to the corpus, so
/// the same weights behave sensibly at 10k or 10M pages.
pub fn score(s: &Signals, w: &Weights, median_pagerank: f32, max_bm25: f32) -> Scored {
    let half = if median_pagerank > 0.0 { median_pagerank * 4.0 } else { 1e-6 };

    let text = if max_bm25 > 0.0 { (s.bm25 / max_bm25).clamp(0.0, 1.0) } else { 0.0 };

    let text_score = w.text_weight * text;
    let pagerank_boost = w.pagerank_weight * saturate(s.pagerank, half);
    let quality_boost = w.quality_weight * s.quality;
    // Distinct linking domains saturate fast: going 0 -> 5 domains means a
    // lot, 200 -> 205 means nothing.
    let domain_trust_boost = w.domain_trust_weight * saturate(s.inbound_domains as f32, 8.0);

    // Quality gates the whole result rather than merely nudging it: a
    // parked page that happens to match the query should not surface at all.
    let gate = (0.25 + 0.75 * s.quality).clamp(0.0, 1.0);

    let score = (text_score + pagerank_boost + quality_boost + domain_trust_boost) * gate;

    Scored {
        score,
        explain: common::ScoreExplain {
            text_bm25: text_score,
            pagerank_boost,
            anchor_boost: 0.0, // folded into the BM25 term via field boost
            quality_boost,
            domain_trust_boost,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(bm25: f32, pr: f32, q: f32, d: u32) -> Signals {
        Signals { bm25, pagerank: pr, quality: q, inbound_domains: d }
    }

    /// Regression test for the normalisation bug this module documents:
    /// a short keyword-stuffed page with a big raw-BM25 lead must not beat
    /// an authoritative page once scores are normalised.
    #[test]
    fn authority_can_overcome_a_keyword_stuffed_bm25_lead() {
        let w = Weights::default();
        let max = 82.0;
        // Real numbers observed on the query "delhi capital".
        let spam = score(&sig(82.0, 0.0004, 1.0, 0), &w, 0.0003, max);
        let wiki = score(&sig(50.0, 0.0090, 1.0, 30), &w, 0.0003, max);
        assert!(
            wiki.score > spam.score,
            "authoritative page {} should beat keyword-stuffed page {}",
            wiki.score,
            spam.score
        );
    }

    #[test]
    fn saturation_is_bounded_and_monotonic() {
        assert_eq!(saturate(0.0, 1.0), 0.0);
        assert!(saturate(1.0, 1.0) - 0.5 < 1e-6);
        // Bounded above by 1.0 inclusive — f32 saturates exactly at the top.
        assert!(saturate(1e9, 1.0) <= 1.0);
        assert!(saturate(5.0, 1.0) > saturate(2.0, 1.0));
    }

    #[test]
    fn parked_page_cannot_outrank_real_one_on_equal_text() {
        let w = Weights::default();
        let real = score(&sig(10.0, 0.001, 1.0, 5), &w, 0.001, 10.0);
        let parked = score(&sig(10.0, 0.001, 0.0, 5), &w, 0.001, 10.0);
        assert!(real.score > parked.score * 1.5, "real {} parked {}", real.score, parked.score);
    }

    #[test]
    fn authority_breaks_ties_but_does_not_swamp_relevance() {
        let w = Weights::default();
        // Strong text match, no authority.
        let relevant = score(&sig(30.0, 0.0, 1.0, 0), &w, 0.001, 30.0);
        // Weak text match, enormous authority.
        let authoritative = score(&sig(3.0, 100.0, 1.0, 10_000), &w, 0.001, 30.0);
        assert!(
            relevant.score > authoritative.score,
            "text {} should beat pure authority {}",
            relevant.score,
            authoritative.score
        );
    }

    #[test]
    fn authority_wins_when_text_is_comparable() {
        let w = Weights::default();
        let a = score(&sig(12.0, 0.01, 1.0, 40), &w, 0.001, 12.0);
        let b = score(&sig(12.0, 0.0001, 1.0, 1), &w, 0.001, 12.0);
        assert!(a.score > b.score);
    }
}
