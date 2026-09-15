//! A web index as a candidate source.
//!
//! The three self-contained sources — hostname synthesis, the OSM places
//! index, Wikidata — each cover a different slice and together still miss.
//! Guessing needs the domain to be derivable from the name. OSM needs
//! somebody to have tagged the POI with a website. Wikidata needs the entity
//! to be notable enough to have an item, and records *organisations*, not
//! pages. None of that reaches "the BCA programme page at Chandigarh
//! University": a deep page on a site whose domain is an abbreviation.
//!
//! A web index reaches it. This module is the seam for borrowing one.
//!
//! What it is *not* is a replacement for the pipeline. Results enter as
//! candidates and are ranked, fetched, checked for parking and dropped if
//! they cannot be confirmed, exactly like a guessed hostname. "The search
//! API's first result" and "a URL we fetched and verified answers your
//! query" are different claims, and only the second reaches the user. That
//! distinction is the reason this is one source among four rather than the
//! answer.
//!
//! ## Providers
//!
//! Whichever credentials are present are used; with none, the resolver runs
//! on its self-contained sources alone and nothing breaks.
//!
//! - `SERPER_API_KEY` — <https://serper.dev>, 2,500 free credits, no card.
//!   Google's index, so query spelling correction comes along with it.
//! - `GOOGLE_CSE_KEY` + `GOOGLE_CSE_CX` — Google's official JSON API, 100
//!   queries/day free. The engine must be set to search the entire web.
//! - `SEARXNG_URL` — a self-hosted SearXNG instance. No signup, no quota,
//!   and no third party holding the query log.
//! - `BRAVE_API_KEY` — <https://brave.com/search/api/>. An independent index
//!   rather than a Google reseller, which is the better story for a company
//!   building search; its free plan requires a card on file.

use crate::DirectoryHit;

/// Which provider to call, resolved from the environment at startup.
#[derive(Debug, Clone)]
pub enum Provider {
    Serper { key: String },
    GoogleCse { key: String, cx: String },
    SearxNg { base_url: String },
    Brave { key: String },
}

impl Provider {
    /// First provider with credentials present, in preference order.
    pub fn from_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

        if let Some(key) = var("SERPER_API_KEY") {
            return Some(Provider::Serper { key });
        }
        if let (Some(key), Some(cx)) = (var("GOOGLE_CSE_KEY"), var("GOOGLE_CSE_CX")) {
            return Some(Provider::GoogleCse { key, cx });
        }
        if let Some(base_url) = var("SEARXNG_URL") {
            return Some(Provider::SearxNg { base_url });
        }
        if let Some(key) = var("BRAVE_API_KEY") {
            return Some(Provider::Brave { key });
        }
        None
    }

    pub fn name(&self) -> &'static str {
        match self {
            Provider::Serper { .. } => "serper",
            Provider::GoogleCse { .. } => "google-cse",
            Provider::SearxNg { .. } => "searxng",
            Provider::Brave { .. } => "brave",
        }
    }
}

/// Web results for a query, best first.
///
/// Returns an empty vec on any failure — a bad key, a rate limit, a network
/// blip. This is one source among several, so an outage should degrade the
/// answer rather than fail the request.
pub async fn search(
    client: &reqwest::Client,
    provider: &Provider,
    query: &str,
    limit: usize,
) -> Vec<DirectoryHit> {
    let result = match provider {
        Provider::Serper { key } => serper(client, key, query, limit).await,
        Provider::GoogleCse { key, cx } => google_cse(client, key, cx, query, limit).await,
        Provider::SearxNg { base_url } => searxng(client, base_url, query, limit).await,
        Provider::Brave { key } => brave(client, key, query, limit).await,
    };
    match result {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!("{} search failed: {e}", provider.name());
            Vec::new()
        }
    }
}

async fn serper(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<DirectoryHit>> {
    let body: serde_json::Value = client
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", key)
        .json(&serde_json::json!({ "q": query, "num": limit }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // `organic` holds the ordinary web results. The `sitelinks` under the
    // first of them often hold exactly the deep page a navigational query
    // wants — the "Admission", "Prospectus", "Online Fee" rows beneath a
    // school's entry — so they are lifted out as candidates in their own
    // right rather than discarded with the wrapper.
    let mut out = Vec::new();
    let empty = Vec::new();
    if let Some(results) = body.get("organic").and_then(|v| v.as_array()) {
        for r in results {
            let Some(url) = r.get("link").and_then(|v| v.as_str()) else { continue };
            let name = r.get("title").and_then(|v| v.as_str()).unwrap_or_default();
            out.push(DirectoryHit {
                name: name.to_string(),
                website: url.to_string(),
                source: "serper".into(),
            });

            for link in r.get("sitelinks").and_then(|v| v.as_array()).unwrap_or(&empty) {
                if let Some(u) = link.get("link").and_then(|v| v.as_str()) {
                    let t = link.get("title").and_then(|v| v.as_str()).unwrap_or_default();
                    out.push(DirectoryHit {
                        name: t.to_string(),
                        website: u.to_string(),
                        source: "serper".into(),
                    });
                }
            }
        }
    }
    out.truncate(limit);
    Ok(out)
}

async fn google_cse(
    client: &reqwest::Client,
    key: &str,
    cx: &str,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<DirectoryHit>> {
    let body: serde_json::Value = client
        .get("https://www.googleapis.com/customsearch/v1")
        .query(&[
            ("key", key),
            ("cx", cx),
            ("q", query),
            ("num", &limit.min(10).to_string()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(items_of(&body, "items", "link", "title", limit))
}

async fn searxng(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<DirectoryHit>> {
    let body: serde_json::Value = client
        .get(format!("{}/search", base_url.trim_end_matches('/')))
        .query(&[("q", query), ("format", "json")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(items_of(&body, "results", "url", "title", limit))
}

async fn brave(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<DirectoryHit>> {
    let body: serde_json::Value = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .query(&[("q", query), ("count", &limit.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let results = body.pointer("/web/results").cloned().unwrap_or_default();
    let wrapper = serde_json::json!({ "results": results });
    Ok(items_of(&wrapper, "results", "url", "title", limit))
}

/// Pull {url, title} pairs out of a JSON array under `key`.
fn items_of(
    body: &serde_json::Value,
    key: &str,
    url_field: &str,
    title_field: &str,
    limit: usize,
) -> Vec<DirectoryHit> {
    body.get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    let url = r.get(url_field)?.as_str()?.to_string();
                    let name = r
                        .get(title_field)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some(DirectoryHit { name, website: url, source: "web".into() })
                })
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_results_from_a_flat_array() {
        let body = serde_json::json!({
            "items": [
                { "link": "https://a.example/x", "title": "A" },
                { "link": "https://b.example/y", "title": "B" },
            ]
        });
        let hits = items_of(&body, "items", "link", "title", 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].website, "https://a.example/x");
        assert_eq!(hits[0].name, "A");
    }

    #[test]
    fn a_missing_or_malformed_payload_yields_nothing_rather_than_panicking() {
        assert!(items_of(&serde_json::json!({}), "items", "link", "title", 5).is_empty());
        let junk = serde_json::json!({ "items": [{ "no_link": 1 }] });
        assert!(items_of(&junk, "items", "link", "title", 5).is_empty());
    }

    #[test]
    fn no_credentials_means_no_provider() {
        // Only meaningful when the environment is clean; skip otherwise so
        // this does not fail on a developer machine that has a key set.
        if std::env::var("SERPER_API_KEY").is_ok()
            || std::env::var("BRAVE_API_KEY").is_ok()
            || std::env::var("SEARXNG_URL").is_ok()
            || std::env::var("GOOGLE_CSE_KEY").is_ok()
        {
            return;
        }
        assert!(Provider::from_env().is_none());
    }
}
