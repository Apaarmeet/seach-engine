//! Finding the right page *within* a site.
//!
//! Half of the navigational problem, and the half a homepage guess cannot
//! touch. "valuepickr bajaj finance" names a site and a page on it; landing
//! on `valuepickr.com` is not the answer, it is the first step. The answer
//! is `forum.valuepickr.com/t/bajaj-finance-limited/267`.
//!
//! The cheap route in is the one the site itself provides. Nearly every
//! non-trivial site publishes a sitemap — it is how they ask to be indexed —
//! and it enumerates their URLs in one document. ValuePickr's is 5,596 URLs
//! in a single 944 KB file, which is one HTTP request against crawling a
//! forum thread by thread. Sites that publish no sitemap simply fall back to
//! the homepage answer; nothing breaks.
//!
//! Matching then happens against the URL slug, because that is all a sitemap
//! carries — no titles, no text. That turns out to be enough, because slugs
//! are written by humans to describe the page.

use crate::query::Token;
use common::robots::RobotsRules;
use std::io::Read;

/// Sitemap documents fetched per site.
///
/// A sitemap index fans out to children; large sites publish dozens. The cap
/// keeps one query from turning into a crawl of somebody's entire archive.
const MAX_SITEMAP_DOCS: usize = 6;

/// URLs kept from a site's sitemaps. Beyond this, matching cost stops being
/// worth the extra recall for a single navigational query.
const MAX_URLS: usize = 60_000;

/// Sitemaps can be large; this is per document, not per site.
const MAX_SITEMAP_BYTES: usize = 12 * 1024 * 1024;

/// A page on the site, with how well its URL matches what was asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct PageMatch {
    pub url: String,
    pub score: f32,
}

/// Fetch robots.txt for a host: needed both for politeness and because it is
/// where sitemaps are declared.
pub async fn robots_for(client: &reqwest::Client, host: &str) -> RobotsRules {
    let url = format!("https://{host}/robots.txt");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            RobotsRules::parse(&body, "rust-search-engine-bot")
        }
        _ => RobotsRules::default(),
    }
}

/// Every URL we can enumerate for a site, via its sitemaps.
///
/// Two concurrent rounds rather than a serial queue. Sitemaps nest exactly
/// one level in practice — an index pointing at documents — and walking that
/// serially means a chain of dependent round trips for files that are often
/// close to a megabyte. ValuePickr's index fans out to two documents;
/// fetching them one after the other doubled the stage for no reason.
pub async fn sitemap_urls(
    client: &reqwest::Client,
    host: &str,
    robots: &RobotsRules,
) -> Vec<String> {
    // Prefer what robots.txt declares; fall back to the conventional
    // locations only when it declares nothing, rather than always probing
    // all three and eating two 404s on every site that is well configured.
    let round1: Vec<String> = if robots.sitemaps.is_empty() {
        ["sitemap.xml", "sitemap_index.xml", "sitemap-index.xml"]
            .iter()
            .map(|p| format!("https://{host}/{p}"))
            .collect()
    } else {
        robots.sitemaps.iter().take(MAX_SITEMAP_DOCS).cloned().collect()
    };

    let mut seen: std::collections::HashSet<String> = round1.iter().cloned().collect();
    let mut urls: Vec<String> = Vec::new();
    let mut children: Vec<String> = Vec::new();

    for (body, _) in fetch_many(client, round1).await {
        sort_locs(&body, &mut urls, &mut children);
    }

    // One level of fan-out, bounded by what is left of the document budget.
    children.retain(|c| seen.insert(c.clone()));
    children.truncate(MAX_SITEMAP_DOCS);
    if !children.is_empty() {
        for (body, _) in fetch_many(client, children).await {
            // Grandchildren are dropped: a sitemap index pointing at another
            // index is legal but vanishingly rare, and chasing it turns one
            // query into an unbounded crawl.
            sort_locs(&body, &mut urls, &mut Vec::new());
        }
    }

    urls.truncate(MAX_URLS);
    urls.sort();
    urls.dedup();
    urls
}

/// Route a document's `<loc>` values by what kind of document it is.
fn sort_locs(body: &str, urls: &mut Vec<String>, children: &mut Vec<String>) {
    let locs = extract_locs(body);
    // A sitemap index contains sitemaps; a sitemap contains pages. The
    // element name distinguishes them, but detecting the wrapper is simpler
    // and survives namespace prefixes.
    if body.contains("<sitemapindex") {
        children.extend(locs);
    } else {
        urls.extend(locs);
    }
}

async fn fetch_many(
    client: &reqwest::Client,
    urls: Vec<String>,
) -> Vec<(String, String)> {
    futures::future::join_all(urls.into_iter().map(|u| {
        let client = client.clone();
        async move { fetch_text(&client, &u).await.map(|body| (body, u)) }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Fetch a sitemap document, transparently gunzipping `.xml.gz`.
///
/// reqwest decompresses `Content-Encoding: gzip` on its own, but gzipped
/// sitemaps are usually served as a gzip *body type* (`Content-Type:
/// application/gzip`), which it will not touch. Without this branch, those
/// sites yield a document full of binary and silently match nothing.
async fn fetch_text(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() > MAX_SITEMAP_BYTES {
        tracing::debug!("sitemap {url} too large ({} bytes)", bytes.len());
        return None;
    }

    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .take(MAX_SITEMAP_BYTES as u64)
            .read_to_string(&mut out)
            .ok()?;
        Some(out)
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Pull `<loc>` values out of a sitemap without a full XML parse.
///
/// A real parser buys nothing here: the grammar of interest is one element,
/// and sitemaps in the wild are frequently malformed in ways that make a
/// strict parser give up on the whole document rather than the bad line.
pub fn extract_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<loc>") {
        rest = &rest[start + 5..];
        let Some(end) = rest.find("</loc>") else { break };
        let raw = rest[..end].trim();
        if !raw.is_empty() {
            out.push(unescape(raw));
        }
        rest = &rest[end + 6..];
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Rank a site's URLs against the page half of the query.
///
/// Two things decide it, and the second is what makes it work.
///
/// **Coverage** — do the asked-for words appear in the slug? Necessary, but
/// nowhere near sufficient: ValuePickr has a dozen URLs containing both
/// "bajaj" and "finance".
///
/// **Concision** — how much of the slug is *not* the query. Of those dozen,
/// `/t/bajaj-finance-limited/267` is three words of which two were asked
/// for, while `/t/bajaj-finance-branches-have-terrible-reviews-in-google-
/// ethical-dilemma-in-investing/87344` is twelve. Both cover the query
/// completely; only one is the thread about Bajaj Finance. A slug that is
/// mostly the query is a page *about* the query — the rest is a page that
/// mentions it.
pub fn best_pages(urls: &[String], page: &[Token], limit: usize) -> Vec<PageMatch> {
    if page.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<PageMatch> = urls
        .iter()
        .filter_map(|url| {
            let slug = slug_tokens(url);
            if slug.is_empty() {
                return None;
            }
            let hits = page
                .iter()
                .filter(|t| t.forms.iter().any(|f| slug.iter().any(|s| s == f)))
                .count();
            if hits == 0 {
                return None;
            }
            let coverage = hits as f32 / page.len() as f32;
            let concision = hits as f32 / slug.len() as f32;
            // Coverage squared: a URL missing one of the asked-for words is
            // much worse than one that has them all, not linearly worse.
            let mut score = coverage * coverage * (0.6 + 0.4 * concision);
            if is_phrase(&slug, page) {
                score *= PHRASE_BONUS;
            }
            Some(PageMatch { url: url.clone(), score: score.min(1.0) })
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Shorter URL breaks ties: canonical pages sit above their own
            // paginated and filtered variants.
            .then_with(|| a.url.len().cmp(&b.url.len()))
    });
    scored.retain(|m| m.score >= MIN_PAGE_MATCH);
    scored.truncate(limit);
    scored
}

/// Floor for a sub-page to be worth fetching at all.
///
/// Without it, "best of N" always returns something: asked for "bajaj
/// finance" against a site that has no such page, the top three were
/// `/tag/shriram-city-union-finance/`, `/tag/shriram-transport-finance-
/// company/` and `/stories/shriram-city-union-finance/` — pages matching on
/// the word "finance" alone. They then scored well enough on the *page* to
/// be returned as the answer, which is the worst failure this system can
/// have: a confident, verified, wrong URL.
///
/// Set above what half-coverage can reach (0.25 x 0.73 = 0.18) and below
/// what full coverage yields even on a wordy slug (1.0 x 0.63 = 0.63), so
/// the rule is effectively "every word you asked for has to be in the slug".
/// A site with no matching page now returns nothing and the homepage answer
/// stands, which is the honest outcome.
pub const MIN_PAGE_MATCH: f32 = 0.35;

/// Multiplier when the asked-for words appear as a phrase in the slug.
///
/// Coverage and concision cannot separate `bajaj-finance-limited` from
/// `bajaj-housing-finance`: both are three words containing both query
/// terms, so both score identically — and they are different companies.
/// What separates them is that one says "bajaj finance" and the other says
/// "bajaj ... finance". Word order and adjacency are free to check and carry
/// real meaning in a slug, because slugs are written as prose.
const PHRASE_BONUS: f32 = 1.25;

/// Do the query's words appear contiguously and in order in the slug?
fn is_phrase(slug: &[String], page: &[Token]) -> bool {
    if page.len() < 2 || slug.len() < page.len() {
        return false;
    }
    slug.windows(page.len()).any(|w| {
        w.iter()
            .zip(page)
            .all(|(word, t)| t.forms.iter().any(|f| f == word))
    })
}

/// Meaningful words in a URL path.
///
/// Numeric segments and one-character segments are dropped: Discourse URLs
/// look like `/t/<slug>/<topic-id>`, and counting `t` and `267` as content
/// dilutes the concision measure that does the real work.
fn slug_tokens(url: &str) -> Vec<String> {
    let path = url::Url::parse(url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| url.to_string());

    path.split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 1 && !s.chars().all(|c| c.is_ascii_digit()))
        .map(|s| s.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;

    fn page_tokens(q: &str) -> Vec<Token> {
        query::parse(q, &|_| false).tokens
    }

    /// Real URLs from ValuePickr's sitemap, including the ones that make
    /// this hard — several threads contain both query words.
    fn valuepickr_urls() -> Vec<String> {
        [
            "https://forum.valuepickr.com/t/bajaj-investment-holding-co/50",
            "https://forum.valuepickr.com/t/bajaj-finance-limited/267",
            "https://forum.valuepickr.com/t/bajaj-hindustan-bottom-formation/471",
            "https://forum.valuepickr.com/t/bajaj-finserv/646",
            "https://forum.valuepickr.com/t/bajaj-consumer-care-ltd/1230",
            "https://forum.valuepickr.com/t/bajaj-auto-is-the-company-back-on-growth-track/23985",
            "https://forum.valuepickr.com/t/bajaj-finance-branches-have-terrbile-reviews-in-google-ethical-dilemma-in-investing/87344",
            "https://forum.valuepickr.com/t/hdfc-bank/121",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The exact target from Vinay's example.
    #[test]
    fn finds_the_bajaj_finance_thread() {
        let best = best_pages(&valuepickr_urls(), &page_tokens("bajaj finance"), 5);
        assert_eq!(best[0].url, "https://forum.valuepickr.com/t/bajaj-finance-limited/267");
    }

    /// The discriminating case: a longer thread covers the query just as
    /// completely, and must still lose.
    #[test]
    fn concision_beats_a_longer_slug_with_the_same_coverage() {
        let best = best_pages(&valuepickr_urls(), &page_tokens("bajaj finance"), 8);
        let target = best.iter().position(|m| m.url.ends_with("/267")).unwrap();
        let verbose = best.iter().position(|m| m.url.contains("terrbile")).unwrap();
        assert!(target < verbose, "verbose thread outranked the canonical one");
    }

    /// `bajaj-finserv` matches "bajaj" but not "finance". Partial coverage
    /// is not a weaker answer, it is a different page — so it is dropped
    /// rather than ranked below the real one.
    #[test]
    fn partial_coverage_is_excluded_not_merely_outranked() {
        let best = best_pages(&valuepickr_urls(), &page_tokens("bajaj finance"), 8);
        assert!(
            !best.iter().any(|m| m.url.ends_with("/646")),
            "bajaj-finserv covers only half the query and should not be offered"
        );
        assert!(best.iter().any(|m| m.url.ends_with("/267")));
    }

    /// The failure that made this threshold necessary: a site with no
    /// matching page must return nothing, not its three least-bad URLs.
    #[test]
    fn a_site_without_the_page_returns_nothing() {
        let urls: Vec<String> = [
            "https://www.valuepickr.com/tag/shriram-city-union-finance/",
            "https://www.valuepickr.com/tag/shriram-transport-finance-company/",
            "https://www.valuepickr.com/stories/shriram-city-union-finance/",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(best_pages(&urls, &page_tokens("bajaj finance"), 3).is_empty());
    }

    /// Two real ValuePickr threads, same word count, both fully covering
    /// the query — and different companies. Only adjacency separates them.
    #[test]
    fn an_adjacent_phrase_beats_the_same_words_split_apart() {
        let urls: Vec<String> = [
            "https://forum.valuepickr.com/t/bajaj-housing-finance/165063",
            "https://forum.valuepickr.com/t/bajaj-finance-limited/267",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let best = best_pages(&urls, &page_tokens("bajaj finance"), 5);
        assert_eq!(best[0].url, "https://forum.valuepickr.com/t/bajaj-finance-limited/267");
    }

    /// A long thread that happens to contain the phrase must still lose to
    /// the short one that is *about* it.
    #[test]
    fn the_phrase_bonus_does_not_override_concision() {
        let best = best_pages(&valuepickr_urls(), &page_tokens("bajaj finance"), 8);
        let target = best.iter().position(|m| m.url.ends_with("/267")).unwrap();
        let verbose = best.iter().position(|m| m.url.contains("terrbile"));
        assert_eq!(target, 0);
        if let Some(v) = verbose {
            assert!(target < v);
        }
    }

    #[test]
    fn unrelated_pages_are_dropped_entirely() {
        let best = best_pages(&valuepickr_urls(), &page_tokens("bajaj finance"), 8);
        assert!(!best.iter().any(|m| m.url.contains("hdfc")));
    }

    #[test]
    fn numeric_and_single_letter_segments_are_not_content() {
        assert_eq!(
            slug_tokens("https://forum.valuepickr.com/t/bajaj-finance-limited/267"),
            vec!["bajaj", "finance", "limited"]
        );
    }

    #[test]
    fn an_empty_page_query_asks_for_nothing() {
        assert!(best_pages(&valuepickr_urls(), &[], 5).is_empty());
    }

    #[test]
    fn extracts_locs_from_a_sitemap() {
        let xml = r#"<?xml version="1.0"?>
            <urlset><url><loc>https://a.com/x</loc></url>
            <url><loc>https://a.com/y?p=1&amp;q=2</loc></url></urlset>"#;
        assert_eq!(
            extract_locs(xml),
            vec!["https://a.com/x", "https://a.com/y?p=1&q=2"]
        );
    }

    /// A truncated document must yield what it has rather than nothing.
    #[test]
    fn malformed_xml_does_not_lose_the_valid_entries() {
        let xml = "<urlset><url><loc>https://a.com/x</loc></url><url><loc>https://a.com/tr";
        assert_eq!(extract_locs(xml), vec!["https://a.com/x"]);
    }
}
