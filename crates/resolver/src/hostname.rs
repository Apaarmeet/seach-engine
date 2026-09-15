//! Guessing hostnames from words.
//!
//! This is the part that reaches sites nothing has indexed. A person who has
//! never heard of St. Edmund's School reasons "a school in Shillong, the
//! domain is probably stedmundshillong.in" and types it to find out. The
//! reasoning is mechanical enough to encode, and the check is cheap enough
//! to run hundreds of times: a DNS lookup costs no page fetch, no crawl
//! budget, and nobody's robots.txt cares.
//!
//! So the strategy is to be *generous* here and let DNS do the filtering.
//! Precision comes from the fact that made-up domains overwhelmingly do not
//! resolve — `stedmundshillong.in` exists, `saintedmundsschoolshillong.org`
//! does not, and that asymmetry is the whole signal.
//!
//! The two decisions that actually matter, both learned from the real
//! target `stedmundshillong.in`:
//!
//!   - **Generic words get dropped.** The domain contains the saint and the
//!     city but not the word "school". Candidates that keep every token miss
//!     it entirely.
//!   - **Both singular and plural must be tried.** The domain is
//!     `st` + `edmund` + `shillong`, but everyone types "edmunds".

use crate::query::{classify, Kind};

/// Suffixes to try, most likely first.
///
/// Deliberately a parameter with a default rather than a constant: the
/// assignment's bar is "a local radio station in Moscow", which lives under
/// `.ru`, and a fixed India-flavoured list would never reach it. The caller
/// sets this from whatever country signal it has — a gazetteer hit on the
/// place token, or the user's locale.
pub const DEFAULT_TLDS: &[&str] =
    &["com", "in", "org", "co.in", "net", "ac.in", "edu.in"];

/// Hard ceiling on generated candidates.
///
/// DNS lookups are cheap but not free, and the tail of the ordered list is
/// junk by construction. Measured on the sample queries, everything that
/// resolves appears well inside the first hundred.
pub const MAX_CANDIDATES: usize = 240;

/// Most surface forms to take per token.
///
/// Unbounded, this multiplies out: four tokens with three forms each is 81
/// bases before TLDs are applied. Two covers every case the equivalence
/// table actually produces (`saint`/`st`, `edmunds`/`edmund`).
const MAX_FORMS_PER_TOKEN: usize = 2;

/// A hostname worth checking, with why we thought of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host: String,
    /// Position in the generated order; lower is a stronger prior. Kept so
    /// downstream ranking can break ties between two hosts that both
    /// resolve without re-deriving the reasoning.
    pub rank: usize,
}

/// Does this token look like the user typed a domain outright?
///
/// "valuepickr.com" and "forum.valuepickr.com" should be tried verbatim
/// before any guessing happens — the user has already done the work.
pub fn looks_like_host(text: &str) -> bool {
    let Some((label, tld)) = text.rsplit_once('.') else {
        return false;
    };
    !label.is_empty()
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Candidate hostnames for one reading of the query, best-first.
pub fn candidates(
    site: &[String],
    is_place: &dyn Fn(&str) -> bool,
    tlds: &[&str],
) -> Vec<Candidate> {
    let mut hosts: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |hosts: &mut Vec<String>, h: String| {
        if seen.insert(h.clone()) {
            hosts.push(h);
        }
    };

    // A query that already contains a domain is not a guess. Emit it first
    // and let it win on evidence like anything else.
    for t in site {
        if looks_like_host(t) {
            push(&mut hosts, t.trim_start_matches("www.").to_string());
        }
    }

    let kinds: Vec<Kind> = site.iter().map(|t| classify(t, is_place)).collect();
    let optional: Vec<usize> = (0..site.len())
        .filter(|&i| matches!(kinds[i], Kind::Generic | Kind::Place))
        .collect();

    // Which tokens are load-bearing enough that a domain must contain one.
    //
    // Normally the distinctive ones. When a query has none, a place name is
    // promoted to stand in for identity: "moscow radio" has no brand token,
    // but `moscowradio.ru` is exactly the guess a person would make, and the
    // assignment's bar is a Moscow radio station. What is *not* promoted is
    // a query of pure category words — "official website" identifies
    // nothing, and guessing `officialwebsite.com` is noise with a cost.
    let identifying = if kinds.iter().any(|k| *k == Kind::Distinctive) {
        Kind::Distinctive
    } else if kinds.iter().any(|k| *k == Kind::Place) {
        Kind::Place
    } else {
        return finish(hosts);
    };

    let optional: Vec<usize> =
        optional.into_iter().filter(|&i| kinds[i] != identifying).collect();

    // Word combinations to try, in mask order (best mask first).
    let combos: Vec<Vec<String>> = ordered_masks(&kinds, &optional)
        .into_iter()
        .flat_map(|mask| {
            let included: Vec<usize> = (0..site.len())
                .filter(|&i| kinds[i] == identifying || mask.contains(&i))
                .collect();
            let form_lists: Vec<Vec<String>> = included
                .iter()
                .map(|&i| {
                    let mut f = crate::query::forms_for(&site[i]);
                    f.truncate(MAX_FORMS_PER_TOKEN);
                    f
                })
                .collect();
            cartesian(&form_lists)
        })
        .filter(|combo| is_plausible_label(&combo.concat()))
        .collect();

    // Two passes, not one interleaved pass. Hyphenated domains are far rarer
    // than concatenated ones, so emitting `st-edmunds-shillong.com` next to
    // `stedmundshillong.com` doubles the cost of reaching the *next* word
    // combination. Measured on the sample query, interleaving pushed the
    // correct host past the probe budget entirely.
    for combo in &combos {
        let joined = combo.concat();
        for tld in tlds {
            push(&mut hosts, format!("{joined}.{tld}"));
        }
    }
    // Initials, but only for runs that look like an institution's name.
    //
    // Institutions are known by their acronym more often than by their full
    // name and register domains to match — JNU, BITS, IIM. A searcher typing
    // the name in full still wants `jnu.ac.in`, and no spelling variation on
    // "jawaharlal nehru university" will ever produce it.
    //
    // The guard is that the run contains a category word ("university",
    // "school", "institute"), because that is what makes a word sequence a
    // *name* rather than a list of topics. Without it, "valuepickr bajaj
    // finance" generated `vbf.com`, `vbf.org`, `vbf.net` and `vb.com` —
    // which all exist, all consumed the probe budget, and pushed
    // `forum.valuepickr.com` out of the run entirely. Three words that
    // happen to sit next to each other are not an acronym.
    let name_like = |combo: &Vec<String>| {
        combo.len() >= 2 && combo.iter().any(|w| crate::query::is_generic_word(w))
    };
    for combo in combos.iter().filter(|c| name_like(c)) {
        let acronym: String =
            combo.iter().filter_map(|w| w.chars().next()).collect();
        if acronym.len() >= 3 {
            for tld in tlds {
                push(&mut hosts, format!("{acronym}.{tld}"));
            }
        }
    }

    for combo in combos.iter().filter(|c| c.len() > 1) {
        let hyphened = combo.join("-");
        for tld in tlds {
            push(&mut hosts, format!("{hyphened}.{tld}"));
        }
    }

    finish(hosts)
}

fn finish(hosts: Vec<String>) -> Vec<Candidate> {
    hosts
        .into_iter()
        .take(MAX_CANDIDATES)
        .enumerate()
        .map(|(rank, host)| Candidate { host, rank })
        .collect()
}

/// Which optional tokens to include, best-first.
///
/// The ordering is the prior learned from real domains: owners drop the
/// category word ("school") far more often than they drop the place
/// ("shillong"), because the place is what disambiguates them from every
/// other St. Edmund's in the world.
fn ordered_masks(kinds: &[Kind], optional: &[usize]) -> Vec<Vec<usize>> {
    // 2^n over optional tokens. Capped because a query with many generic and
    // place words is already a poor navigational query, and the combinations
    // are worth less than the budget they consume.
    const MAX_OPTIONAL: usize = 4;
    let optional: Vec<usize> = optional.iter().copied().take(MAX_OPTIONAL).collect();

    let mut masks: Vec<Vec<usize>> = (0..(1u32 << optional.len()))
        .map(|bits| {
            optional
                .iter()
                .enumerate()
                .filter(|(b, _)| bits & (1 << b) != 0)
                .map(|(_, &i)| i)
                .collect()
        })
        .collect();

    masks.sort_by_key(|mask: &Vec<usize>| {
        let kept_places =
            mask.iter().filter(|&&i| kinds[i] == Kind::Place).count();
        let kept_generics =
            mask.iter().filter(|&&i| kinds[i] == Kind::Generic).count();
        // Sort key is minimised: keep places, drop generics, and prefer
        // shorter hostnames when those two agree.
        (kept_generics, usize::MAX - kept_places, mask.len())
    });
    masks
}

/// Is this a hostname label anyone would actually register?
fn is_plausible_label(s: &str) -> bool {
    (3..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn cartesian(lists: &[Vec<String>]) -> Vec<Vec<String>> {
    lists.iter().fold(vec![Vec::new()], |acc, list| {
        acc.iter()
            .flat_map(|prefix| {
                list.iter().map(move |item| {
                    let mut next = prefix.clone();
                    next.push(item.clone());
                    next
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> impl Fn(&str) -> bool {
        |w: &str| matches!(w, "shillong" | "moscow" | "bangalore")
    }

    fn hosts(site: &[&str]) -> Vec<String> {
        let site: Vec<String> = site.iter().map(|s| s.to_string()).collect();
        candidates(&site, &places(), DEFAULT_TLDS)
            .into_iter()
            .map(|c| c.host)
            .collect()
    }

    /// The target from Vinay's example. Getting here requires three separate
    /// things to work: saint->st, edmunds->edmund, and dropping "school".
    #[test]
    fn reaches_the_real_st_edmunds_domain() {
        let h = hosts(&["saint", "edmunds", "school", "shillong"]);
        assert!(
            h.contains(&"stedmundshillong.in".to_string()),
            "missing stedmundshillong.in; first 20 were {:?}",
            &h[..20.min(h.len())]
        );
    }

    #[test]
    fn keeps_the_place_and_drops_the_category_word_first() {
        let h = hosts(&["saint", "edmunds", "school", "shillong"]);
        let with_place = h.iter().position(|x| x == "stedmundshillong.in").unwrap();
        let with_category = h
            .iter()
            .position(|x| x == "stedmundschoolshillong.in")
            .unwrap_or(usize::MAX);
        assert!(
            with_place < with_category,
            "dropping 'school' should be tried before keeping it"
        );
    }

    #[test]
    fn single_brand_token_produces_the_obvious_domain() {
        let h = hosts(&["valuepickr"]);
        assert_eq!(h[0], "valuepickr.com");
        assert!(h.contains(&"valuepickr.in".to_string()));
    }

    #[test]
    fn a_typed_domain_is_tried_verbatim_and_first() {
        let h = hosts(&["forum.valuepickr.com"]);
        assert_eq!(h[0], "forum.valuepickr.com");
    }

    #[test]
    fn www_is_stripped_from_a_typed_domain() {
        let h = hosts(&["www.valuepickr.com"]);
        assert_eq!(h[0], "valuepickr.com");
    }

    #[test]
    fn single_words_are_never_hyphenated() {
        let h = hosts(&["valuepickr"]);
        assert!(!h.iter().any(|x| x.contains('-')));
    }

    #[test]
    fn multi_word_sites_get_a_hyphenated_form() {
        let h = hosts(&["value", "pickr"]);
        assert!(h.contains(&"value-pickr.com".to_string()));
        assert!(h.contains(&"valuepickr.com".to_string()));
    }

    /// A query with no identifying word must not spray guesses.
    #[test]
    fn pure_category_queries_generate_nothing_useful() {
        let h = hosts(&["official", "website"]);
        assert!(
            !h.contains(&"officialwebsite.com".to_string()),
            "generic-only query should not be treated as a brand"
        );
    }

    /// The assignment's stated bar. No brand token, so the place has to be
    /// promoted to identity or this query produces nothing at all.
    #[test]
    fn a_place_stands_in_for_identity_when_nothing_else_does() {
        let site = vec!["moscow".to_string(), "radio".to_string()];
        let h: Vec<String> = candidates(&site, &places(), &["ru"])
            .into_iter()
            .map(|c| c.host)
            .collect();
        assert!(h.contains(&"moscowradio.ru".to_string()), "got {h:?}");
        assert!(h.contains(&"moscow.ru".to_string()), "got {h:?}");
    }

    /// Concatenated forms must all be exhausted before any hyphenated one,
    /// or the budget runs out before the likelier spellings are reached.
    #[test]
    fn hyphenated_forms_come_after_every_plain_form() {
        let h = hosts(&["saint", "edmunds", "school", "shillong"]);
        let first_hyphen = h.iter().position(|x| x.contains('-')).unwrap_or(h.len());
        let last_plain = h.iter().rposition(|x| !x.contains('-')).unwrap_or(0);
        assert!(last_plain < first_hyphen, "hyphenated hosts interleaved with plain ones");
    }

    /// Institutions are known by their initials and register accordingly.
    #[test]
    fn initials_are_tried_for_multi_word_names() {
        let h = hosts(&["jawaharlal", "nehru", "university"]);
        assert!(h.contains(&"jnu.com".to_string()), "no acronym candidate");
    }

    /// The regression this guard exists for: three unrelated words are not
    /// an institution's initials, and `vbf.com` exists.
    #[test]
    fn a_topic_list_does_not_produce_an_acronym() {
        let h = hosts(&["valuepickr", "bajaj", "finance"]);
        assert!(!h.contains(&"vbf.com".to_string()), "invented an acronym");
        assert!(!h.contains(&"vb.com".to_string()));
    }

    #[test]
    fn single_word_names_produce_no_acronym() {
        let h = hosts(&["valuepickr"]);
        assert!(!h.iter().any(|x| x.starts_with("v.")));
    }

    #[test]
    fn candidate_count_is_bounded() {
        let h = hosts(&["saint", "edmunds", "higher", "secondary", "school", "shillong"]);
        assert!(h.len() <= MAX_CANDIDATES, "got {}", h.len());
    }

    #[test]
    fn tld_list_is_caller_controlled_for_non_indian_targets() {
        let site = vec!["echo".to_string(), "moscow".to_string()];
        let h: Vec<String> = candidates(&site, &places(), &["ru", "com"])
            .into_iter()
            .map(|c| c.host)
            .collect();
        assert!(h.contains(&"echomoscow.ru".to_string()));
        assert!(!h.iter().any(|x| x.ends_with(".in")));
    }

    #[test]
    fn rejects_implausible_labels() {
        assert!(!is_plausible_label("ab"));
        assert!(!is_plausible_label(&"x".repeat(41)));
        assert!(is_plausible_label("stedmundshillong"));
    }
}
