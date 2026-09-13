//! Query side of local search.
//!
//! The ranking question here is different from web search. On the web you
//! ask "which page best answers this?". Locally you ask "which of these
//! nearby things do I want?", and *nearby* is doing enormous work — a
//! perfect name match 40 km away is usually a worse answer than a rough
//! match 300 m away.

use crate::geo;
use crate::poi::Poi;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyResult {
    #[serde(flatten)]
    pub poi: Poi,
    pub distance_m: f64,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<NearbyExplain>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NearbyExplain {
    pub text_match: f32,
    pub proximity: f32,
    pub prominence: f32,
}

#[derive(Debug, Clone)]
pub struct NearbyWeights {
    pub text_weight: f32,
    pub proximity_weight: f32,
    pub prominence_weight: f32,
    /// Distance at which desirability falls to 1/e. Set from the search
    /// radius rather than fixed, so "within 500 m" and "within 20 km" both
    /// produce sensible gradients.
    pub decay_fraction: f64,
}

impl Default for NearbyWeights {
    fn default() -> Self {
        Self {
            // Proximity outweighs text here, unlike web search. For a
            // category query ("cafe near me") every candidate already
            // matches the category, so text carries little information and
            // distance is what the user is actually choosing on.
            text_weight: 0.8,
            proximity_weight: 1.0,
            prominence_weight: 0.3,
            decay_fraction: 0.4,
        }
    }
}

/// Score one candidate.
pub fn score(
    text_match: f32,
    distance_m: f64,
    prominence: f32,
    radius_m: f64,
    w: &NearbyWeights,
) -> (f32, NearbyExplain) {
    let scale = (radius_m * w.decay_fraction).max(50.0);
    let proximity = geo::distance_decay(distance_m, scale) as f32;

    let text = w.text_weight * text_match;
    let prox = w.proximity_weight * proximity;
    let prom = w.prominence_weight * prominence;

    (
        text + prox + prom,
        NearbyExplain { text_match: text, proximity: prox, prominence: prom },
    )
}

/// Words that qualify a search without identifying anything.
///
/// "cheap food near me" is a category query with an adjective attached, but
/// "cheap" was being treated as the distinctive core — so every result was
/// filtered out for not being named "Cheap". Modifiers must not drive
/// poor-match suppression.
const MODIFIERS: &[&str] = &[
    "cheap", "best", "good", "top", "great", "nice", "affordable", "budget",
    "expensive", "luxury", "new", "old", "open", "24x7", "nearby", "local",
    "famous", "popular", "big", "small", "quick", "fast",
];

/// The part of a query that actually identifies a specific place.
///
/// Category words and place names are stripped, because neither identifies
/// anything: "hospital" describes a kind, "Bangalore" describes where. What
/// remains — "apollo", "iim", "phoenix" — is the distinctive core.
///
/// This distinction is what makes poor-match suppression workable. Judging
/// by whole-query overlap fails both ways: "IIM Bangalore" scored 0.5
/// against "Bangalore Thindies" purely on the city name, while "Christian
/// Medical College" looked like a category query because it contains
/// "medical" and "college".
pub fn distinctive_terms(query: &str, is_place_name: &dyn Fn(&str) -> bool) -> Vec<String> {
    let words: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect();

    // Words consumed by a multi-word category phrase. "pump" alone is not a
    // category word, but in "petrol pump" it carries no identity either.
    let mut in_phrase = vec![false; words.len()];
    for i in 0..words.len().saturating_sub(1) {
        let phrase = format!("{} {}", words[i], words[i + 1]);
        if crate::synonyms::phrase_concept(&phrase).is_some() {
            in_phrase[i] = true;
            in_phrase[i + 1] = true;
        }
    }

    words
        .iter()
        .enumerate()
        .filter(|(i, _)| !in_phrase[*i])
        .map(|(_, w)| w.clone())
        .filter(|w| w.len() > 2)
        .filter(|w| !MODIFIERS.contains(&w.as_str()))
        .filter(|w| !crate::synonyms::is_category_word(w))
        .filter(|w| !is_place_name(w))
        .collect()
}

/// Does a candidate's name contain the query's distinctive terms?
///
/// Returns 1.0 when the query has no distinctive core at all (a pure
/// category query), so suppression never fires on those.
pub fn distinctive_match(name: &str, brand: &str, distinctive: &[String]) -> f32 {
    if distinctive.is_empty() {
        return 1.0;
    }
    let haystack = format!("{} {}", name.to_lowercase(), brand.to_lowercase());
    let hits = distinctive.iter().filter(|w| haystack.contains(w.as_str())).count();
    hits as f32 / distinctive.len() as f32
}

/// Fraction of distinctive terms a result must match to be shown.
///
/// A *majority*, not merely half. At exactly 0.5 a two-term query passes on
/// one match, which is how "Zzyzx Memorial Hospital" matched "Rangadore
/// Memorial Hospital" on the word "memorial" alone. Requiring strictly more
/// than half means a two-term name must match both, while longer names still
/// tolerate a missing word.
pub const MIN_DISTINCTIVE_MATCH: f32 = 0.5;

/// Whether a candidate matches enough of the query's distinctive core.
pub fn distinctive_match_ok(name: &str, brand: &str, distinctive: &[String]) -> bool {
    if distinctive.is_empty() {
        return true;
    }
    distinctive_match(name, brand, distinctive) > MIN_DISTINCTIVE_MATCH
}

/// Detect whether a query is asking for something local.
///
/// Matters because the same words mean different things: "pizza" typed into
/// a general search box may want recipes, while "pizza near me" certainly
/// wants a shop. Getting this wrong in either direction is very visible —
/// showing restaurants for "history of pizza" looks broken.
pub fn has_local_intent(query: &str) -> bool {
    const MARKERS: &[&str] = &[
        "near me", "nearby", "near by", "around me", "close to me", "closest",
        "nearest", "near here", "in my area", "walking distance", "around here",
    ];
    let q = query.to_lowercase();
    MARKERS.iter().any(|m| q.contains(m))
}

/// Strip locality markers so they don't pollute the text match — "cafe near
/// me" should search for "cafe", not for documents containing "near" and "me".
pub fn strip_local_markers(query: &str) -> String {
    const MARKERS: &[&str] = &[
        "near me", "nearby", "near by", "around me", "close to me", "closest",
        "nearest", "near here", "in my area", "walking distance", "around here",
        "near", "around",
    ];
    let mut q = query.to_lowercase();
    for m in MARKERS {
        q = q.replace(m, " ");
    }
    q.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> impl Fn(&str) -> bool {
        |w: &str| matches!(w, "bangalore" | "bengaluru" | "chennai" | "delhi")
    }

    #[test]
    fn modifiers_do_not_become_the_distinctive_core() {
        assert!(distinctive_terms("cheap food", &places()).is_empty());
        assert!(distinctive_terms("best restaurants", &places()).is_empty());
        assert!(distinctive_terms("affordable hotel in chennai", &places()).is_empty());
    }

    #[test]
    fn pure_category_queries_have_no_distinctive_core() {
        // Suppression must never fire on these.
        assert!(distinctive_terms("cafe", &places()).is_empty());
        assert!(distinctive_terms("pharmacy", &places()).is_empty());
        assert!(distinctive_terms("petrol pump", &places()).is_empty());
        assert!(distinctive_terms("hospital in chennai", &places()).is_empty());
    }

    #[test]
    fn named_queries_keep_only_the_identifying_word() {
        assert_eq!(distinctive_terms("IIM Bangalore", &places()), vec!["iim"]);
        assert_eq!(distinctive_terms("Apollo Hospital", &places()), vec!["apollo"]);
        assert_eq!(distinctive_terms("Phoenix Mall", &places()), vec!["phoenix"]);
        assert_eq!(
            distinctive_terms("Christian Medical College", &places()),
            vec!["christian"]
        );
    }

    /// The exact failures this machinery exists to prevent.
    #[test]
    fn wrong_results_score_below_the_threshold() {
        let iim = distinctive_terms("IIM Bangalore", &places());
        assert!(distinctive_match("Bangalore Thindies", "", &iim) < MIN_DISTINCTIVE_MATCH);

        let cmc = distinctive_terms("Christian Medical College", &places());
        assert!(distinctive_match("Apollo Pharmacy", "", &cmc) < MIN_DISTINCTIVE_MATCH);

        let phoenix = distinctive_terms("Phoenix Mall", &places());
        assert!(distinctive_match("One Night in Bangkok", "", &phoenix) < MIN_DISTINCTIVE_MATCH);
    }

    #[test]
    fn genuine_matches_clear_the_threshold() {
        let apollo = distinctive_terms("Apollo Hospital", &places());
        assert!(distinctive_match_ok("Apollo Speciality Hospital", "", &apollo));

        let starbucks = distinctive_terms("Starbucks", &places());
        assert!(distinctive_match_ok("Starbucks", "Starbucks", &starbucks));
    }

    /// One shared word out of two is not a match.
    #[test]
    fn a_single_shared_word_is_not_enough() {
        let zzyzx = distinctive_terms("Zzyzx Memorial Hospital", &places());
        assert_eq!(zzyzx.len(), 2, "expected zzyzx + memorial, got {zzyzx:?}");
        assert!(!distinctive_match_ok("Rangadore Memorial Hospital", "", &zzyzx));
    }

    #[test]
    fn category_queries_are_never_suppressed() {
        let none = distinctive_terms("cafe", &places());
        assert_eq!(distinctive_match("Any Random Place", "", &none), 1.0);
    }

    #[test]
    fn detects_local_intent() {
        assert!(has_local_intent("coffee near me"));
        assert!(has_local_intent("NEAREST ATM"));
        assert!(has_local_intent("pharmacy nearby"));
        assert!(!has_local_intent("history of coffee"));
        assert!(!has_local_intent("indian railways"));
    }

    #[test]
    fn strips_markers_leaving_the_subject() {
        assert_eq!(strip_local_markers("coffee near me"), "coffee");
        assert_eq!(strip_local_markers("nearest atm"), "atm");
        assert_eq!(strip_local_markers("pharmacy nearby"), "pharmacy");
    }

    #[test]
    fn closer_place_wins_when_text_and_prominence_tie() {
        let w = NearbyWeights::default();
        let (near, _) = score(1.0, 200.0, 0.5, 2000.0, &w);
        let (far, _) = score(1.0, 1800.0, 0.5, 2000.0, &w);
        assert!(near > far, "near {near} should beat far {far}");
    }

    /// Proximity should dominate, but not absolutely — a much better,
    /// well-documented match slightly further away can still win.
    #[test]
    fn a_better_match_can_beat_a_marginally_closer_one() {
        let w = NearbyWeights::default();
        let (good_bit_far, _) = score(1.0, 400.0, 1.0, 2000.0, &w);
        let (poor_bit_near, _) = score(0.1, 300.0, 0.0, 2000.0, &w);
        assert!(good_bit_far > poor_bit_near);
    }

    /// ...but distance must not be overwhelmed by text on a category query.
    #[test]
    fn distance_dominates_across_large_gaps() {
        let w = NearbyWeights::default();
        let (near_weak, _) = score(0.5, 150.0, 0.0, 3000.0, &w);
        let (far_perfect, _) = score(1.0, 12_000.0, 1.0, 3000.0, &w);
        assert!(
            near_weak > far_perfect,
            "150m ({near_weak}) should beat a perfect match 12km away ({far_perfect})"
        );
    }

    #[test]
    fn radius_scales_the_decay() {
        let w = NearbyWeights::default();
        // 1 km is far within a 2 km search, but close within a 50 km one.
        let (tight, _) = score(1.0, 1000.0, 0.0, 2000.0, &w);
        let (wide, _) = score(1.0, 1000.0, 0.0, 50_000.0, &w);
        assert!(wide > tight);
    }
}
