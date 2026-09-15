//! Shared types passed between crawler -> signals -> indexer -> ranker -> api.
//!
//! Everything here is designed around one idea: sharding by URL hash.
//! That's what lets this go from "one laptop" to "a fleet of machines"
//! without changing the data model — only how many shards you run.

pub mod robots;

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// An outbound link *with the text the author used to describe it*.
///
/// Carrying the anchor text is the whole point. Brin & Page's original
/// insight was that link text describes the target page better than the
/// target's own copy does — "download firefox" points at mozilla.org even
/// though that page may never use the phrase. Dropping this (keeping only
/// the href) throws away one of the strongest relevance signals there is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutLink {
    pub url: String,
    /// Visible anchor text, whitespace-normalised, possibly empty.
    #[serde(default)]
    pub text: String,
}

/// A single crawled page, as written by the crawler (one JSON line per page)
/// and consumed by the signals pass and the indexer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawledPage {
    pub url: String,
    pub title: String,
    pub body_text: String,
    /// Absolute links this page points at, with their anchor text. Used to
    /// build the link graph (authority) *and* the inbound-anchor index.
    #[serde(default)]
    pub links: Vec<OutLink>,
    pub crawled_at_unix: u64,
    /// HTTP status of the fetch, so the indexer can skip non-200s.
    pub status: u16,
}

impl CrawledPage {
    /// Just the link targets — what the link-graph/PageRank pass needs.
    pub fn outlink_urls(&self) -> impl Iterator<Item = &str> {
        self.links.iter().map(|l| l.url.as_str())
    }
}

/// Everything the offline enrichment pass derives about a page. Kept in one
/// struct so the indexer does a single lookup per document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageSignals {
    pub url: String,
    /// Link-graph authority (PageRank).
    pub pagerank: f32,
    /// Concatenated anchor text of *inbound* links, deduplicated.
    pub anchor_text: String,
    /// How many distinct domains link here — a cheap, hard-to-fake trust
    /// signal that PageRank alone doesn't expose.
    pub inbound_domains: u32,
    /// 0.0 = junk (parked/boilerplate/thin), 1.0 = clean content.
    pub quality: f32,
    /// SimHash of the body, for near-duplicate detection.
    pub simhash: u64,
    /// True when another, higher-quality page has an near-identical body.
    pub is_duplicate: bool,
}

/// Stable shard assignment for a URL. Crawler workers, index segments, and
/// the query fan-out in the api all use this same function, so a given URL
/// always lands in the same shard everywhere in the pipeline.
pub fn shard_for_url(url: &str, num_shards: u32) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    (hasher.finish() % num_shards as u64) as u32
}

/// Registrable-ish domain for a URL. Not a full PSL implementation — it
/// keeps the last two labels, plus three for common multi-part suffixes like
/// `co.in` / `co.uk` so `foo.co.in` and `bar.co.in` aren't treated as one
/// site. Swap in the `publicsuffix` crate if you need this exact.
pub fn registrable_domain(url: &str) -> Option<String> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Some(host);
    }
    const MULTI: [&str; 8] = ["co", "com", "net", "org", "gov", "ac", "edu", "res"];
    let take = if labels.len() >= 3 && MULTI.contains(&labels[labels.len() - 2]) {
        3
    } else {
        2
    };
    Some(labels[labels.len() - take..].join("."))
}

/// Canonical form of a URL, for duplicate detection.
///
/// Near-duplicate detection by content alone is not enough: `10net.in/x` and
/// `www.10net.in/x` serve the same page with tiny textual differences (a
/// counter, a timestamp), which pushes their SimHashes just past the
/// similarity threshold and lets both into the results. Normalising the URL
/// catches this class directly and cheaply.
///
/// Applied: lowercase host, drop a leading `www.`, drop the default port,
/// drop tracking parameters, sort remaining parameters, and strip a trailing
/// slash and common index filenames.
pub fn canonical_url(url: &str) -> String {
    const TRACKING: &[&str] = &[
        "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
        "fbclid", "gclid", "msclkid", "ref", "referrer", "source",
    ];

    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.trim_end_matches('/').to_lowercase();
    };

    parsed.set_fragment(None);

    if let Some(host) = parsed.host_str() {
        let host = host.to_ascii_lowercase();
        let stripped = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let _ = parsed.set_host(Some(&stripped));
    }
    // http and https of the same page are the same page.
    let _ = parsed.set_scheme("https");
    let _ = parsed.set_port(None);

    let mut params: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !TRACKING.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    params.sort();
    if params.is_empty() {
        parsed.set_query(None);
    } else {
        let joined = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        parsed.set_query(Some(&joined));
    }

    let mut path = parsed.path().to_string();
    for index in ["index.html", "index.htm", "index.php", "default.aspx"] {
        if let Some(base) = path.strip_suffix(index) {
            path = base.to_string();
            break;
        }
    }
    if path.len() > 1 {
        path = path.trim_end_matches('/').to_string();
    }
    parsed.set_path(&path);

    parsed.to_string().trim_end_matches('/').to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
    /// Per-signal breakdown, populated when the query asks to explain.
    /// Being able to answer "why did this rank here?" is the difference
    /// between a search engine you can tune and one you can only guess at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain: Option<ScoreExplain>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoreExplain {
    pub text_bm25: f32,
    pub pagerank_boost: f32,
    pub anchor_boost: f32,
    pub quality_boost: f32,
    pub domain_trust_boost: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub took_ms: u64,
    pub total_hits: usize,
    pub results: Vec<SearchResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_url_collapses_www_and_scheme_variants() {
        let a = canonical_url("http://www.10net.in/category/weather");
        let b = canonical_url("https://10net.in/category/weather/");
        assert_eq!(a, b, "www/non-www and http/https must canonicalise together");
    }

    #[test]
    fn canonical_url_drops_tracking_params_and_sorts_the_rest() {
        let a = canonical_url("https://x.in/p?utm_source=fb&b=2&a=1");
        let b = canonical_url("https://x.in/p?a=1&b=2");
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_url_strips_index_files_and_fragments() {
        let a = canonical_url("https://x.in/dir/index.html#top");
        let b = canonical_url("https://x.in/dir/");
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_url_keeps_genuinely_different_pages_apart() {
        assert_ne!(canonical_url("https://x.in/a"), canonical_url("https://x.in/b"));
        assert_ne!(canonical_url("https://x.in/p?id=1"), canonical_url("https://x.in/p?id=2"));
        // A different site is not a duplicate just because the path matches.
        assert_ne!(canonical_url("https://a.in/p"), canonical_url("https://b.in/p"));
    }

    #[test]
    fn canonical_url_survives_unparseable_input() {
        assert_eq!(canonical_url("not a url"), "not a url");
    }

    #[test]
    fn registrable_domain_handles_multipart_suffixes() {
        assert_eq!(registrable_domain("https://www.foo.co.in/x").unwrap(), "foo.co.in");
        assert_eq!(registrable_domain("https://a.b.example.com/").unwrap(), "example.com");
        assert_eq!(registrable_domain("https://flipkart.in/").unwrap(), "flipkart.in");
    }
}
