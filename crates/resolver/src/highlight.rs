//! Pointing at the control, not just the page.
//!
//! Landing on the right page is only half of what someone wanted. They asked
//! "flipkart customer service" because they intend to *click something*, and
//! a help centre is a wall of links. The universal workaround is Ctrl+F —
//! which means the last step of nearly every navigational search is manual.
//!
//! Browsers already solve this and almost nobody uses it. A URL ending in
//! `#:~:text=Customer%20Service` opens the page, scrolls to those words and
//! highlights them — a W3C feature shipped in Chrome, Edge and Safari. It is
//! just a URL: no extension, no automation, nothing clicking on the user's
//! behalf. That last point matters, because the actions people most want
//! help with are changing passwords and editing DNS records, and an agent
//! that gets those wrong locks someone out of their own account.
//!
//! The page has already been downloaded to verify it, so this costs no extra
//! request.
//!
//! The one rule that cannot be broken: **never emit a fragment without
//! checking the text is really on the page.** A fragment that does not match
//! fails *silently* — the browser loads the page normally, nothing
//! highlights, and the user concludes the product is broken. A wrong
//! highlight is worse than no highlight, so an unverified guess is not an
//! option.

use crate::query::{Kind, Token};

/// Longest control label worth highlighting.
///
/// Controls are labels, not sentences. "Customer Service" is a button;
/// "Contact our customer service team about your recent order" is a
/// paragraph that happens to contain the words. Long fragments are also
/// fragile — they break when inline markup splits the text differently from
/// how it was extracted.
const MAX_LABEL_CHARS: usize = 60;

/// Shortest label worth highlighting. Below this, matches are coincidental.
const MIN_LABEL_CHARS: usize = 3;

/// Does this label look like a control rather than prose or markup debris?
///
/// Both rules come from measured silent failures — fragments that were
/// emitted, looked plausible, and matched nothing in the browser:
///
///   - `"change a user's password."` ends in a full stop. Buttons do not.
///     That was a sentence lifted out of a paragraph, and the page in
///     question renders almost nothing server-side anyway.
///   - `"Computing (BCA/ MCA)"` has a space after the slash. That spacing is
///     an artefact of how text nodes sit in the markup, and it disappears
///     once the browser lays the page out — so the fragment can never match.
///
/// Question marks are deliberately allowed: FAQ accordions are real controls
/// and `"How is pricing calculated for the paid plans?"` highlights correctly
/// on Notion's pricing page.
fn looks_like_a_control(label: &str) -> bool {
    if label.ends_with('.') {
        return false;
    }
    // Space adjacent to punctuation that should hug its neighbours.
    const ARTEFACTS: &[&str] = &["/ ", " /", "( ", " )", " ,", " :", "  "];
    !ARTEFACTS.iter().any(|a| label.contains(a))
}

/// Minimum score before a highlight is offered at all.
const MIN_SCORE: f32 = 0.5;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Highlight {
    /// The exact visible text that will be highlighted.
    pub text: String,
    /// The page URL with the text fragment appended.
    pub url: String,
}

/// Every control label on the page, for an LLM to choose between.
///
/// Returned as a menu rather than letting a model read the whole page and
/// write an answer: the model picks one of these strings verbatim, so it
/// cannot invent a control that isn't there. A hallucinated fragment fails
/// silently in the browser, which the user reads as a broken product.
pub fn candidate_labels(html: &str, page_text: &str, limit: usize) -> Vec<String> {
    let doc = scraper::Html::parse_document(html);
    let haystack = normalise(page_text);
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (selector, _) in CANDIDATE_SOURCES {
        let Ok(sel) = scraper::Selector::parse(selector) else { continue };
        for el in doc.select(&sel) {
            let label = normalise(&el.text().collect::<String>());
            if label.chars().count() < MIN_LABEL_CHARS
                || label.chars().count() > MAX_LABEL_CHARS
            {
                continue;
            }
            // Same verification as the heuristic path: only offer labels that
            // demonstrably appear in the page's own text, and that look like
            // controls rather than prose.
            if !looks_like_a_control(&label)
                || !haystack.contains(&label)
                || !seen.insert(label.clone())
            {
                continue;
            }
            out.push(label);
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}

/// Build a highlight for a label already known to be on the page.
pub fn from_label(page_url: &str, page_text: &str, label: &str) -> Option<Highlight> {
    let label = normalise(label);
    if label.is_empty() || !looks_like_a_control(&label) {
        return None;
    }
    let text = shorten(&label);
    if !normalise(page_text).contains(&text) {
        return None;
    }
    Some(Highlight { url: fragment_url(page_url, &text), text })
}

/// Find the control matching `wanted` and build a highlighting URL.
///
/// Returns `None` whenever nothing scores well or the text cannot be
/// confirmed on the page — the caller then shows the plain URL, which is
/// still a correct answer.
pub fn find(
    html: &str,
    page_url: &str,
    page_text: &str,
    wanted: &[Token],
) -> Option<Highlight> {
    if wanted.is_empty() || html.is_empty() {
        return None;
    }

    let doc = scraper::Html::parse_document(html);
    let haystack = normalise(page_text);

    let mut best: Option<(f32, String)> = None;
    for (selector, weight) in CANDIDATE_SOURCES {
        let Ok(sel) = scraper::Selector::parse(selector) else { continue };
        for el in doc.select(&sel) {
            let label = normalise(&el.text().collect::<String>());
            if label.chars().count() < MIN_LABEL_CHARS
                || label.chars().count() > MAX_LABEL_CHARS
            {
                continue;
            }
            if !looks_like_a_control(&label) {
                continue;
            }
            let score = score_label(&label, wanted) * weight;
            if score <= MIN_SCORE {
                continue;
            }
            // Verified here, not at the end: a label assembled from nested
            // markup may not appear contiguously in the page's own text, and
            // that is exactly the case that would fail silently in a browser.
            if !haystack.contains(&label) {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((score, label));
            }
        }
    }

    let (_, label) = best?;
    let text = shorten(&label);
    // Re-verified: shortening must not produce a phrase the page lacks.
    if !haystack.contains(&text) {
        return None;
    }
    Some(Highlight { url: fragment_url(page_url, &text), text })
}

/// Where controls live, and how much to trust each.
///
/// A control is nearly always a link or a button. Headings are included
/// because settings pages label sections with them ("DNS Records"), but
/// weighted below interactive elements — the thing a person wants to click
/// beats the thing that names the area it sits in.
const CANDIDATE_SOURCES: &[(&str, f32)] = &[
    ("a", 1.0),
    ("button", 1.0),
    ("[role=button]", 1.0),
    ("summary", 0.95),
    ("label", 0.9),
    ("h1, h2, h3, h4", 0.8),
];

/// How well a label answers the request.
///
/// Coverage decides whether it is the right control; concision decides
/// whether it is the control rather than a sentence mentioning it. Both are
/// needed: "Customer Service" and "Contact our customer service team about
/// your recent order" both cover the query completely, and only one is a
/// button.
fn score_label(label: &str, wanted: &[Token]) -> f32 {
    let words: Vec<&str> = label.split_whitespace().collect();
    if words.is_empty() {
        return 0.0;
    }
    let lower: Vec<String> = words.iter().map(|w| strip_punct(w)).collect();

    let hits = wanted
        .iter()
        .filter(|t| t.forms.iter().any(|f| lower.iter().any(|w| w == f)))
        .count();
    if hits == 0 {
        return 0.0;
    }

    let coverage = hits as f32 / wanted.len() as f32;
    let concision = hits as f32 / words.len() as f32;
    coverage * coverage * (0.55 + 0.45 * concision)
}

fn strip_punct(w: &str) -> String {
    w.to_lowercase()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string()
}

/// Which words of the query describe the control being looked for.
///
/// Everything naming the *site* has to go, or the highlight lands on the
/// logo. Derived by subtraction rather than from any one reading of the
/// query: an earlier version took the "page" half of the best site/page
/// split, and because that split can legitimately be read backwards —
/// "customer service" as the site, "flipkart" as the page — it highlighted
/// the word *Flipkart* in the header of Flipkart's own help centre. Likewise
/// "GitHub Docs" on GitHub's docs, and "About Chandigarh" on Chandigarh
/// University.
///
/// So four things are removed, each for the same reason — they identify the
/// site, not the control:
///   - anything spelled out in the hostname (`flipkart` in flipkart.com)
///   - anything in the entity's recorded name
///   - place names
///   - words naming a kind of organisation ("university", "school")
///
/// What survives is the request: "customer service", "dns records",
/// "billing settings", "bca".
pub fn control_words(
    all_tokens: &[Token],
    host: &str,
    entity_name: Option<&str>,
) -> Vec<Token> {
    let host = host.to_lowercase();
    let name = entity_name.unwrap_or("").to_lowercase();

    all_tokens
        .iter()
        .filter(|t| matches!(t.kind, Kind::Distinctive | Kind::Generic))
        .filter(|t| !crate::query::names_an_organisation(&t.text))
        .filter(|t| !t.forms.iter().any(|f| host.contains(f.as_str())))
        .filter(|t| !t.forms.iter().any(|f| name.contains(f.as_str())))
        .cloned()
        .collect()
}

/// Reduce a label to the shortest phrase that still identifies it.
///
/// Text fragments match a *substring* of the rendered text, so a shorter
/// fragment is strictly more likely to match — and the mismatches measured in
/// the browser were all trailing decoration that the page renders differently
/// from how it is served:
///
/// ```text
///   served:   "Bachelor of Computer Applications (BCA)"
///   rendered: "Bachelor of Computer Applications"
/// ```
///
/// The parenthetical is markup the layout drops. Trimming it turns a silent
/// failure into a hit, and costs nothing: the remaining phrase is still
/// unique on the page.
///
/// Capped at a few words for the same reason — every extra word is another
/// chance for the browser's text to differ from the markup's.
fn shorten(label: &str) -> String {
    const MAX_WORDS: usize = 6;

    // Drop a trailing bracketed part: "(BCA)", "[PDF]", "(opens in new tab)".
    let trimmed = match label.rfind(['(', '[']) {
        Some(i) if i > 0 => label[..i].trim_end(),
        _ => label,
    };
    let trimmed = trimmed.trim_end_matches([',', ':', ';', '-', '–', '—', ' ']);

    let words: Vec<&str> = trimmed.split_whitespace().take(MAX_WORDS).collect();
    if words.is_empty() {
        label.to_string()
    } else {
        words.join(" ")
    }
}

/// Append a text fragment to a URL.
pub fn fragment_url(page_url: &str, text: &str) -> String {
    // Any existing fragment is replaced: a URL cannot carry two.
    let base = page_url.split('#').next().unwrap_or(page_url);
    format!("{base}#:~:text={}", encode(text))
}

/// Percent-encode text for a fragment directive.
///
/// `-`, `,` and `&` are *syntax* in this format — they separate the prefix,
/// start, end and suffix parts of the directive. Leaving them raw inside the
/// text silently changes what the browser looks for, so the encoding here is
/// stricter than ordinary URL escaping: everything outside the unreserved
/// set is escaped.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for b in text.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;

    fn words(q: &str) -> Vec<Token> {
        query::parse(q, &|_| false).tokens
    }

    const HELP_PAGE: &str = r#"
        <html><body>
          <header><a href="/">Flipkart</a></header>
          <nav>
            <a href="/orders">Your Orders</a>
            <a href="/helpcentre/contact">Customer Service</a>
            <a href="/returns">Returns &amp; Refunds</a>
          </nav>
          <p>Contact our customer service team about your recent order
             if you need any help at all with this.</p>
          <script>var customerService = "Customer Service";</script>
        </body></html>
    "#;

    fn page_text() -> String {
        // What probe::extract would produce: visible text, no script.
        "Flipkart Your Orders Customer Service Returns & Refunds Contact our \
         customer service team about your recent order if you need any help \
         at all with this."
            .to_string()
    }

    #[test]
    fn finds_the_control_and_not_the_sentence() {
        let h = find(HELP_PAGE, "https://flipkart.com/helpcentre", &page_text(),
                     &words("customer service"))
            .expect("should find a control");
        assert_eq!(h.text, "Customer Service");
    }

    #[test]
    fn builds_a_valid_text_fragment() {
        let h = find(HELP_PAGE, "https://flipkart.com/helpcentre", &page_text(),
                     &words("customer service"))
            .unwrap();
        assert_eq!(
            h.url,
            "https://flipkart.com/helpcentre#:~:text=Customer%20Service"
        );
    }

    /// `-` and `,` delimit the directive's own parts. Left raw they change
    /// what the browser searches for, and the highlight silently misses.
    #[test]
    fn encodes_the_characters_that_are_fragment_syntax() {
        assert_eq!(encode("Pay-as-you-go"), "Pay%2Das%2Dyou%2Dgo");
        assert_eq!(encode("Billing, Plans"), "Billing%2C%20Plans");
        assert_eq!(encode("Terms & Conditions"), "Terms%20%26%20Conditions");
    }

    /// The rule that must never break: no fragment for text that is not
    /// demonstrably on the page, because the failure is invisible.
    #[test]
    fn never_emits_a_fragment_for_text_absent_from_the_page() {
        // The markup offers the label, but the page text does not contain it
        // — as happens when markup splits a phrase across elements.
        let absent = "Flipkart Your Orders Returns & Refunds";
        assert!(find(HELP_PAGE, "https://flipkart.com/x", absent,
                     &words("customer service"))
            .is_none());
    }

    #[test]
    fn returns_nothing_when_no_control_matches() {
        assert!(find(HELP_PAGE, "https://flipkart.com/x", &page_text(),
                     &words("dns records"))
            .is_none());
    }

    /// Script contents are text nodes per the HTML spec, so a naive walk
    /// finds "Customer Service" inside a JavaScript string literal.
    #[test]
    fn does_not_highlight_text_from_a_script_tag() {
        let script_only = r#"<html><body>
            <script>var x = "Customer Service";</script></body></html>"#;
        assert!(find(script_only, "https://x.com/", &page_text(),
                     &words("customer service"))
            .is_none());
    }

    #[test]
    fn replaces_an_existing_fragment_rather_than_appending() {
        assert_eq!(
            fragment_url("https://x.com/page#section", "Billing"),
            "https://x.com/page#:~:text=Billing"
        );
    }

    /// A label the model returns but the page does not contain must be
    /// rejected — the whole point of constraining it to a menu.
    #[test]
    fn a_label_absent_from_the_page_is_rejected() {
        assert!(from_label("https://x.com/", &page_text(), "Delete Account").is_none());
        assert!(from_label("https://x.com/", &page_text(), "Customer Service").is_some());
    }

    #[test]
    fn candidate_labels_exclude_script_contents() {
        let labels = candidate_labels(HELP_PAGE, &page_text(), 50);
        assert!(labels.iter().any(|l| l == "Customer Service"));
        assert!(labels.iter().all(|l| !l.contains("var ")));
    }

    /// The two shapes that produced silent failures in the browser.
    #[test]
    fn prose_and_markup_debris_are_not_controls() {
        assert!(!looks_like_a_control("change a user's password."));
        assert!(!looks_like_a_control("Computing (BCA/ MCA)"));
        assert!(!looks_like_a_control("Billing  Plans"));
    }

    /// FAQ accordions are genuine controls and highlight correctly.
    #[test]
    fn a_question_heading_is_still_a_control() {
        assert!(looks_like_a_control(
            "How is pricing calculated for the paid plans?"
        ));
        assert!(looks_like_a_control("DNS records"));
        assert!(looks_like_a_control("Customer Service"));
    }

    /// The measured mismatch: served markup carries a parenthetical the
    /// rendered page drops, so the full label never matches in a browser.
    #[test]
    fn trailing_parentheticals_are_trimmed() {
        assert_eq!(
            shorten("Bachelor of Computer Applications (BCA)"),
            "Bachelor of Computer Applications"
        );
        assert_eq!(shorten("Download brochure [PDF]"), "Download brochure");
        assert_eq!(shorten("DNS records"), "DNS records");
    }

    #[test]
    fn very_long_labels_are_capped() {
        let long = "How is pricing calculated for the paid plans exactly";
        assert_eq!(shorten(long).split_whitespace().count(), 6);
    }

    #[test]
    fn shortening_never_produces_an_unverifiable_fragment() {
        // "Customer Service (support)" shortens to text the page does have.
        assert!(from_label("https://x.com/", &page_text(), "Customer Service (support)").is_some());
        // But a label whose *shortened* form is absent is still rejected.
        assert!(from_label("https://x.com/", &page_text(), "Delete Account (now)").is_none());
    }

    #[test]
    fn an_empty_request_produces_no_highlight() {
        assert!(find(HELP_PAGE, "https://x.com/", &page_text(), &[]).is_none());
    }

    /// The site's own name appears in the header of every page, so
    /// highlighting it answers nothing. This is the bug that produced
    /// "Flipkart" as the highlight on Flipkart's help centre.
    #[test]
    fn the_site_name_is_stripped_from_the_control_words() {
        let c = control_words(
            &words("flipkart customer service"),
            "www.flipkart.com",
            None,
        );
        let texts: Vec<&str> = c.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["customer", "service"]);
    }

    #[test]
    fn organisation_and_place_words_are_stripped_too() {
        let is_place = |w: &str| w == "chandigarh";
        let tokens = query::parse("chandigarh university bca", &is_place).tokens;
        let c = control_words(&tokens, "www.cuchd.in", Some("Chandigarh"));
        let texts: Vec<&str> = c.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["bca"], "only the page request should survive");
    }

    #[test]
    fn a_hostname_spelling_out_the_brand_strips_it() {
        let c = control_words(&words("github billing settings"), "docs.github.com", None);
        let texts: Vec<&str> = c.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["billing", "settings"]);
    }
}
