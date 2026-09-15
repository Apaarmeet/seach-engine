//! Third-party lookup: Wikidata's "official website" property.
//!
//! Hostname synthesis and the OSM directory between them still leave a hole,
//! and it is a structural one. Guessing reaches domains derivable from the
//! name; OSM reaches places someone tagged with a website. Neither reaches
//! Chandigarh University, whose domain is `cuchd.in` — not derivable, and
//! not tagged.
//!
//! Wikidata does. It is a structured database of entities where property
//! P856 is *official website*, it is free, keyless, and it covers precisely
//! the notable organisations whose domains are opaque abbreviations:
//!
//! ```text
//!   Chandigarh University -> cuchd.in
//!   IISER Mohali          -> iisermohali.ac.in
//!   Echo of Moscow        -> echo.msk.ru
//! ```
//!
//! It is emphatically *not* a search engine, and using it is not the same as
//! proxying one. It answers "what is this organisation's website", which is
//! one field of one record — the resolver still has to decide which reading
//! of the query names an entity, still has to find the right page within the
//! site, and still verifies every answer by fetching it. A stale P856 value
//! pointing at a lapsed domain loses to a guess that actually serves the
//! site, because both arrive at the same confidence test.
//!
//! Swapping in a commercial search API (Brave, Bing, SerpAPI) means writing
//! another function with this signature. The pipeline does not change.

use crate::DirectoryHit;

const API: &str = "https://www.wikidata.org/w/api.php";

/// Official websites for entities matching `name`, best first.
///
/// Two round trips by design: Wikidata's search endpoint returns entity ids
/// without their claims, so the properties come from a second call that
/// batches every candidate id at once.
pub async fn official_sites(
    client: &reqwest::Client,
    name: &str,
    limit: usize,
) -> Vec<DirectoryHit> {
    let Some(ids) = search_entities(client, name, limit).await else {
        return Vec::new();
    };
    if ids.is_empty() {
        return Vec::new();
    }
    websites_for(client, &ids).await.unwrap_or_default()
}

async fn search_entities(
    client: &reqwest::Client,
    name: &str,
    limit: usize,
) -> Option<Vec<String>> {
    let resp = client
        .get(API)
        .query(&[
            ("action", "wbsearchentities"),
            ("search", name),
            ("language", "en"),
            ("uselang", "en"),
            ("type", "item"),
            ("format", "json"),
            ("limit", &limit.to_string()),
        ])
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    Some(
        body.get("search")?
            .as_array()?
            .iter()
            .filter_map(|e| e.get("id")?.as_str().map(str::to_string))
            .collect(),
    )
}

async fn websites_for(
    client: &reqwest::Client,
    ids: &[String],
) -> Option<Vec<DirectoryHit>> {
    let resp = client
        .get(API)
        .query(&[
            ("action", "wbgetentities"),
            ("ids", &ids.join("|")),
            ("props", "claims|labels"),
            ("languages", "en"),
            ("format", "json"),
        ])
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let entities = body.get("entities")?.as_object()?;

    // Preserve the search ranking: `wbgetentities` returns a map, whose
    // iteration order has nothing to do with how well each entity matched.
    let mut out = Vec::new();
    for id in ids {
        let Some(entity) = entities.get(id) else { continue };
        let label = entity
            .pointer("/labels/en/value")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        // P856 is "official website".
        let Some(claims) = entity.pointer("/claims/P856").and_then(|v| v.as_array())
        else {
            continue;
        };
        for claim in claims {
            if let Some(url) = claim
                .pointer("/mainsnak/datavalue/value")
                .and_then(|v| v.as_str())
            {
                out.push(DirectoryHit {
                    name: label.clone(),
                    website: url.to_string(),
                    source: "wikidata".into(),
                });
                break;
            }
        }
    }
    Some(out)
}
