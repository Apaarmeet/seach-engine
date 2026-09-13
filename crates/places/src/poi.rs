//! Points of interest: the data model and where they come from.
//!
//! Source is OpenStreetMap. Two ingest paths, deliberately:
//!   - **Overpass API** for a single city — small, fast, good for iterating.
//!   - **PBF extract** for a whole country — 1.7 GB for India, the real thing.
//! Both produce the same `Poi`, so everything downstream is identical.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poi {
    pub id: String,
    pub name: String,
    /// Chain/brand name, when tagged separately from `name`.
    ///
    /// Worth indexing on its own: OSM often tags a franchise as
    /// `name="Tata Starbucks"` with `brand="Starbucks"`, or gives a branch a
    /// local name and puts the chain only in `brand`. Searching just `name`
    /// misses those, and chain lookups ("starbucks near me") are among the
    /// most common local queries there are.
    #[serde(default)]
    pub brand: String,
    /// Human-readable category ("cafe", "hospital", "atm").
    pub category: String,
    /// Broad group used for query matching ("food", "health", "money").
    pub group: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub opening_hours: String,
    /// 0..1 completeness proxy. OSM has no ratings, so this stands in for
    /// prominence — see `prominence()`.
    #[serde(default)]
    pub prominence: f32,
}

/// OSM tag keys that indicate "this node is a place a person might go".
/// Ordered by specificity: the first match wins.
const PLACE_KEYS: &[&str] = &["amenity", "shop", "tourism", "leisure", "healthcare", "office"];

/// Map an OSM category onto a coarse group, so "cafe", "restaurant" and
/// "fast_food" can all answer a query for "food near me".
pub fn group_for(category: &str) -> &'static str {
    match category {
        "restaurant" | "cafe" | "fast_food" | "bar" | "pub" | "food_court" | "ice_cream"
        | "bakery" | "confectionery" => "food",
        "hospital" | "clinic" | "doctors" | "pharmacy" | "dentist" | "veterinary" => "health",
        "atm" | "bank" | "bureau_de_change" | "money_transfer" => "money",
        "fuel" | "charging_station" | "car_repair" | "car_wash" | "parking" => "vehicle",
        "school" | "college" | "university" | "library" | "kindergarten" => "education",
        "supermarket" | "convenience" | "grocery" | "greengrocer" | "mall" | "department_store" => {
            "grocery"
        }
        "hotel" | "guest_house" | "hostel" | "motel" | "apartment" => "stay",
        "police" | "fire_station" | "post_office" | "townhall" | "courthouse" => "civic",
        "bus_station" | "railway_station" | "airport" | "taxi" | "ferry_terminal" => "transport",
        "park" | "garden" | "playground" | "sports_centre" | "fitness_centre" | "gym"
        | "swimming_pool" | "stadium" => "recreation",
        "temple" | "place_of_worship" | "mosque" | "church" | "gurudwara" => "worship",
        "museum" | "attraction" | "viewpoint" | "zoo" | "theme_park" | "artwork" => "attraction",
        _ => "other",
    }
}

/// Completeness as a stand-in for prominence.
///
/// OSM carries no popularity or rating data, so ranking "which cafe is the
/// good one" is not directly possible. What *is* available: places someone
/// bothered to fully describe (hours, phone, website, address) are
/// disproportionately real, operating businesses rather than drive-by pins.
/// It's a weak proxy and should be replaced the moment real signals exist —
/// review counts, check-ins, or click-through from your own users.
pub fn prominence(p: &Poi) -> f32 {
    let mut score = 0.0f32;
    if !p.website.is_empty() {
        score += 0.35;
    }
    if !p.phone.is_empty() {
        score += 0.25;
    }
    if !p.opening_hours.is_empty() {
        score += 0.20;
    }
    if !p.address.is_empty() {
        score += 0.20;
    }
    score.clamp(0.0, 1.0)
}

/// Extract a (category, group) pair from OSM tags, or None if this element
/// isn't a place a user would search for.
pub fn categorise<'a, I>(tags: I) -> Option<(String, &'static str)>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let tags: Vec<(&str, &str)> = tags.into_iter().collect();
    for key in PLACE_KEYS {
        if let Some((_, value)) = tags.iter().find(|(k, _)| k == key) {
            // `yes` is a placeholder value carrying no information.
            if *value == "yes" {
                continue;
            }
            return Some((value.to_string(), group_for(value)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poi(website: &str, phone: &str, hours: &str, addr: &str) -> Poi {
        Poi {
            id: "n1".into(),
            name: "Test".into(),
            brand: String::new(),
            category: "cafe".into(),
            group: "food".into(),
            lat: 12.97,
            lon: 77.59,
            address: addr.into(),
            phone: phone.into(),
            website: website.into(),
            opening_hours: hours.into(),
            prominence: 0.0,
        }
    }

    #[test]
    fn groups_cluster_related_categories() {
        assert_eq!(group_for("cafe"), "food");
        assert_eq!(group_for("restaurant"), "food");
        assert_eq!(group_for("pharmacy"), "health");
        assert_eq!(group_for("atm"), "money");
        assert_eq!(group_for("something_unknown"), "other");
    }

    #[test]
    fn prominence_rewards_completeness() {
        let bare = prominence(&poi("", "", "", ""));
        let full = prominence(&poi("x.com", "123", "Mo-Fr", "MG Road"));
        assert_eq!(bare, 0.0);
        assert!((full - 1.0).abs() < 1e-6);
        assert!(prominence(&poi("x.com", "", "", "")) < full);
    }

    #[test]
    fn categorise_picks_the_most_specific_tag() {
        let tags = vec![("name", "Cafe X"), ("amenity", "cafe")];
        let (cat, group) = categorise(tags).unwrap();
        assert_eq!(cat, "cafe");
        assert_eq!(group, "food");
    }

    #[test]
    fn categorise_skips_placeholder_yes_values() {
        // `shop=yes` says something is a shop but not what kind; if a real
        // category exists further down the list, prefer it.
        let tags = vec![("shop", "yes"), ("tourism", "museum")];
        let (cat, _) = categorise(tags).unwrap();
        assert_eq!(cat, "museum");
    }

    #[test]
    fn categorise_rejects_non_places() {
        let tags = vec![("highway", "residential"), ("name", "MG Road")];
        assert!(categorise(tags).is_none());
    }
}
