//! On-demand Overpass fetching, shared by the CLI builder and the API.

use crate::poi::{self, Poi};
use anyhow::{Context, Result};

pub const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
pub const USER_AGENT: &str = "rsearch-places/0.1 (+https://github.com/local/rsearch)";

/// Build the Overpass QL for "named places within radius".
pub fn query_for(lat: f64, lon: f64, radius_m: f64, limit: usize) -> String {
    format!(
        r#"[out:json][timeout:60];
(
  node(around:{r},{lat},{lon})["amenity"]["name"];
  node(around:{r},{lat},{lon})["shop"]["name"];
  node(around:{r},{lat},{lon})["tourism"]["name"];
  node(around:{r},{lat},{lon})["leisure"]["name"];
  node(around:{r},{lat},{lon})["healthcare"]["name"];
  node(around:{r},{lat},{lon})["amenity"]["brand"];
  node(around:{r},{lat},{lon})["shop"]["brand"];
);
out body {limit};"#,
        r = radius_m as i64
    )
}

/// Parse an Overpass JSON response into POIs.
pub fn parse_response(json: &serde_json::Value) -> Vec<Poi> {
    let elements = match json["elements"].as_array() {
        Some(e) => e,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for el in elements {
        let (Some(lat), Some(lon)) = (el["lat"].as_f64(), el["lon"].as_f64()) else {
            continue;
        };
        let tags = &el["tags"];
        // Fall back to brand when a franchise node carries no name.
        let name = tags["name"].as_str()
            .or_else(|| tags["brand"].as_str());
        let Some(name) = name else { continue };

        let pairs: Vec<(&str, &str)> = tags
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s)))
                    .collect()
            })
            .unwrap_or_default();
        let Some((category, group)) = poi::categorise(pairs) else { continue };

        out.push(Poi {
            id: format!("n{}", el["id"].as_i64().unwrap_or(0)),
            name: name.to_string(),
            brand: first_tag(tags, &["brand", "operator"]),
            category,
            group: group.to_string(),
            lat,
            lon,
            address: ["addr:housenumber", "addr:street", "addr:city"]
                .iter()
                .filter_map(|k| tags[*k].as_str())
                .collect::<Vec<_>>()
                .join(", "),
            phone: first_tag(tags, &["phone", "contact:phone"]),
            website: first_tag(tags, &["website", "contact:website"]),
            opening_hours: first_tag(tags, &["opening_hours"]),
            prominence: 0.0,
        });
    }
    out
}

fn first_tag(tags: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| tags[*k].as_str())
        .unwrap_or("")
        .to_string()
}

/// Async fetch for use inside the API request path.
pub async fn fetch_area(
    client: &reqwest::Client,
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> Result<Vec<Poi>> {
    let body = query_for(lat, lon, radius_m, 20_000);
    let resp = client
        .post(OVERPASS_URL)
        .body(body)
        .send()
        .await
        .context("overpass request failed")?
        .error_for_status()
        .context("overpass returned an error status")?;
    let json: serde_json::Value = resp.json().await.context("overpass returned non-JSON")?;
    Ok(parse_response(&json))
}

/// Blocking variant for the CLI builder.
pub fn fetch_area_blocking(lat: f64, lon: f64, radius_m: f64) -> Result<Vec<Poi>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(180))
        .build()?;
    let json: serde_json::Value = client
        .post(OVERPASS_URL)
        .body(query_for(lat, lon, radius_m, 20_000))
        .send()
        .context("overpass request")?
        .error_for_status()
        .context("overpass error status")?
        .json()?;
    Ok(parse_response(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_embeds_the_radius_and_centre() {
        let q = query_for(12.9716, 77.5946, 1500.0, 100);
        assert!(q.contains("around:1500,12.9716,77.5946"));
        assert!(q.contains("[out:json]"));
    }

    #[test]
    fn parses_a_realistic_response() {
        let json = serde_json::json!({
            "elements": [
                {"type":"node","id":1,"lat":12.97,"lon":77.59,
                 "tags":{"name":"Cafe X","amenity":"cafe","phone":"123"}},
                // No name -> skipped: an unnamed node is not something a
                // person can search for.
                {"type":"node","id":2,"lat":12.98,"lon":77.60,
                 "tags":{"amenity":"cafe"}},
                // Not a place category -> skipped.
                {"type":"node","id":3,"lat":12.99,"lon":77.61,
                 "tags":{"name":"MG Road","highway":"residential"}}
            ]
        });
        let pois = parse_response(&json);
        assert_eq!(pois.len(), 1);
        assert_eq!(pois[0].name, "Cafe X");
        assert_eq!(pois[0].category, "cafe");
        assert_eq!(pois[0].group, "food");
        assert_eq!(pois[0].phone, "123");
    }

    /// Chain queries ("starbucks near me") are among the most common local
    /// searches, and OSM frequently puts the chain only in `brand` — either
    /// alongside a local `name`, or with no `name` at all.
    #[test]
    fn brand_is_captured_and_used_as_a_name_fallback() {
        let json = serde_json::json!({
            "elements": [
                // Local name, chain in brand.
                {"type":"node","id":10,"lat":12.97,"lon":77.59,
                 "tags":{"name":"Tata Starbucks Forum","amenity":"cafe","brand":"Starbucks"}},
                // No name at all — brand must stand in, or it is dropped.
                {"type":"node","id":11,"lat":12.98,"lon":77.60,
                 "tags":{"amenity":"cafe","brand":"Starbucks"}}
            ]
        });
        let pois = parse_response(&json);
        assert_eq!(pois.len(), 2, "brand-only node was dropped");
        assert_eq!(pois[0].brand, "Starbucks");
        assert_eq!(pois[1].name, "Starbucks", "brand should fill in for a missing name");
    }

    #[test]
    fn empty_or_malformed_response_yields_nothing_rather_than_erroring() {
        assert!(parse_response(&serde_json::json!({})).is_empty());
        assert!(parse_response(&serde_json::json!({"elements": []})).is_empty());
    }
}
