//! Place-name geocoding: "cafes in Koramangala" -> coordinates.
//!
//! Built from the same OSM extract as the POI index rather than calling a
//! geocoding API. OSM tags 312,274 named `place=*` nodes across India —
//! cities, towns, suburbs, neighbourhoods, villages — which is everything a
//! local search needs, available offline and with no rate limit.
//!
//! Why this matters beyond convenience: before it existed, "cafes in
//! Koramangala" silently returned cafes near whatever coordinates the client
//! happened to send. Not an error, just quietly the wrong answer — the worst
//! failure mode a search engine has.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Place {
    pub name: String,
    pub kind: String,
    pub lat: f64,
    pub lon: f64,
    /// From the OSM `population` tag when present; 0 otherwise. Used only to
    /// break ties between same-named places.
    #[serde(default)]
    pub population: u64,
    /// ISO 3166-1 alpha-2 country code, lowercase, when known.
    ///
    /// Empty for entries from the OSM extract, which carries no country tag
    /// on `place=` nodes. Populated by the GeoNames world ingest, where it
    /// is the direct answer to a question the URL resolver has to ask
    /// constantly: a site for something in Moscow is probably under `.ru`,
    /// and guessing `.in` for it wastes the entire candidate budget.
    #[serde(default)]
    pub country: String,
}

impl Place {
    /// How wide a search should be when someone names this place.
    ///
    /// A city query wants the whole city; a neighbourhood query wants a few
    /// streets. Using one radius for both makes "in Mumbai" miss most of
    /// Mumbai, or "in Koramangala" spill across Bengaluru.
    pub fn default_radius_m(&self) -> f64 {
        match self.kind.as_str() {
            // Country/state answers are necessarily coarse; the radius is
            // capped by the API anyway, so this is "around the centroid".
            "country" => 50_000.0,
            "state" => 50_000.0,
            "city" => 12_000.0,
            "town" => 6_000.0,
            "suburb" | "quarter" | "borough" => 3_000.0,
            "neighbourhood" | "city_block" => 2_000.0,
            "village" => 2_500.0,
            "hamlet" | "isolated_dwelling" => 1_500.0,
            _ => 2_500.0,
        }
    }

    /// Preference when several places share a name. Bigger settlement types
    /// win, because someone typing a bare name almost always means the
    /// best-known one — there are dozens of villages called "Rampur", and
    /// none of them is what a user means by default.
    fn rank(&self) -> u8 {
        match self.kind.as_str() {
            "country" => 11,
            "state" => 10,
            "city" => 9,
            "town" => 8,
            "suburb" => 7,
            "borough" | "quarter" => 6,
            "neighbourhood" => 5,
            "village" => 4,
            "locality" => 3,
            "hamlet" => 2,
            _ => 1,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Gazetteer {
    /// normalised name -> candidates sharing that name
    by_name: HashMap<String, Vec<Place>>,
}

/// Lowercase, strip punctuation, collapse whitespace. Deliberately crude —
/// it only has to make user input and OSM names meet in the middle.
pub fn normalise(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Gazetteer {
    /// Seed country- and state-level names.
    ///
    /// These are tagged as boundary *relations* in OSM, not `place=` nodes,
    /// so the node-only ingest never sees them. Without this, "hospital in
    /// india" silently fell back to the user's coordinates. The radii are
    /// deliberately large and the results correspondingly coarse — it is
    /// better to answer the question asked, roughly, than a different
    /// question precisely.
    pub fn seed_admin_areas(&mut self) {
        const ADMIN: &[(&str, &str, f64, f64)] = &[
            ("India", "country", 22.35, 78.67),
            // Delhi, Chandigarh and Goa are administratively states/UTs but
            // are metro-sized in practice. Tagging them "state" would make
            // "starbucks in delhi" decline to answer a perfectly reasonable
            // question.
            ("Delhi", "city", 28.6139, 77.2090),
            ("New Delhi", "city", 28.6139, 77.2090),
            ("Chandigarh", "city", 30.7333, 76.7794),
            ("Karnataka", "state", 15.32, 75.71),
            ("Maharashtra", "state", 19.75, 75.71),
            ("Tamil Nadu", "state", 11.13, 78.66),
            ("Kerala", "state", 10.85, 76.27),
            ("Telangana", "state", 18.11, 79.02),
            ("Gujarat", "state", 22.26, 71.19),
            ("Rajasthan", "state", 27.02, 74.22),
            ("Uttar Pradesh", "state", 26.85, 80.95),
            ("West Bengal", "state", 22.99, 87.85),
            ("Punjab", "state", 31.15, 75.34),
            ("Haryana", "state", 29.06, 76.09),
            ("Bihar", "state", 25.10, 85.31),
            ("Odisha", "state", 20.95, 85.10),
            ("Assam", "state", 26.20, 92.94),
            ("Goa", "state", 15.30, 74.12),
        ];
        // Former / colloquial city names. OSM carries only the current
        // official spelling, but people type the old one constantly — and
        // an unrecognised city name is worse than cosmetic: it gets treated
        // as part of the *place's name*, so "IIM Bangalore" matched
        // "Bangalore Thindies" on the city word alone.
        const ALIASES: &[(&str, &str, f64, f64)] = &[
            ("Bangalore", "city", 12.9716, 77.5946),
            ("Madras", "city", 13.0827, 80.2707),
            ("Bombay", "city", 19.0760, 72.8777),
            ("Calcutta", "city", 22.5726, 88.3639),
            ("Poona", "city", 18.5204, 73.8567),
            ("Baroda", "city", 22.3072, 73.1812),
            ("Mysore", "city", 12.2958, 76.6394),
            ("Trivandrum", "city", 8.5241, 76.9366),
            ("Cochin", "city", 9.9312, 76.2673),
            ("Gurgaon", "city", 28.4595, 77.0266),
            ("Pondicherry", "city", 11.9416, 79.8083),
            ("Simla", "city", 31.1048, 77.1734),
            ("Banaras", "city", 25.3176, 82.9739),
            ("Benares", "city", 25.3176, 82.9739),
            ("Allahabad", "city", 25.4358, 81.8463),
            ("Cawnpore", "city", 26.4499, 80.3319),
            ("Mangalore", "city", 12.9141, 74.8560),
            ("Belgaum", "city", 15.8497, 74.4977),
            ("Hubli", "city", 15.3647, 75.1240),
            ("Vizag", "city", 17.6868, 83.2185),
            ("Trichy", "city", 10.7905, 78.7047),
            ("Ooty", "town", 11.4102, 76.6950),
        ];
        for (name, kind, lat, lon) in ALIASES {
            self.insert_authoritative(Place {
                name: (*name).to_string(),
                kind: (*kind).to_string(),
                lat: *lat,
                lon: *lon,
                population: 0,
                country: "in".to_string(),
            });
        }

        // Well-known metro neighbourhoods.
        //
        // OSM tags these as boundary *relations*, which a node-only ingest
        // never sees — so "cafes in Bandra" resolved to a village of the
        // same name in Madhya Pradesh, 1,000 km from Mumbai. These are the
        // areas people actually name when searching in Indian cities.
        const NEIGHBOURHOODS: &[(&str, f64, f64)] = &[
            // Mumbai
            ("Bandra", 19.0596, 72.8295), ("Andheri", 19.1136, 72.8697),
            ("Colaba", 18.9067, 72.8147), ("Juhu", 19.1075, 72.8263),
            ("Powai", 19.1176, 72.9060), ("Dadar", 19.0178, 72.8478),
            ("Worli", 19.0176, 72.8162), ("Malad", 19.1868, 72.8484),
            ("Borivali", 19.2307, 72.8567), ("Lower Parel", 18.9960, 72.8258),
            // Delhi NCR
            ("Connaught Place", 28.6315, 77.2167), ("Saket", 28.5245, 77.2066),
            ("Hauz Khas", 28.5494, 77.2001), ("Dwarka", 28.5921, 77.0460),
            ("Rohini", 28.7495, 77.0565), ("Karol Bagh", 28.6519, 77.1909),
            ("Lajpat Nagar", 28.5677, 77.2433), ("Vasant Kunj", 28.5200, 77.1591),
            // Bengaluru
            ("Whitefield", 12.9698, 77.7500), ("Jayanagar", 12.9250, 77.5938),
            ("HSR Layout", 12.9116, 77.6474), ("Electronic City", 12.8452, 77.6602),
            ("Marathahalli", 12.9591, 77.6974), ("Malleshwaram", 13.0035, 77.5709),
            ("Hebbal", 13.0358, 77.5970), ("BTM Layout", 12.9166, 77.6101),
            // Chennai
            ("T Nagar", 13.0418, 80.2341), ("Adyar", 13.0012, 80.2565),
            ("Velachery", 12.9756, 80.2207), ("Anna Nagar", 13.0850, 80.2101),
            ("Mylapore", 13.0339, 80.2619),
            // Hyderabad
            ("Banjara Hills", 17.4126, 78.4392), ("Jubilee Hills", 17.4326, 78.4071),
            ("Gachibowli", 17.4401, 78.3489), ("Hitech City", 17.4435, 78.3772),
            ("Madhapur", 17.4483, 78.3915), ("Secunderabad", 17.4399, 78.4983),
            // Kolkata
            ("Salt Lake", 22.5800, 88.4200), ("Park Street", 22.5530, 88.3520),
            ("Howrah", 22.5958, 88.2636), ("Ballygunge", 22.5270, 88.3660),
            // Pune
            ("Koregaon Park", 18.5362, 73.8939), ("Hinjewadi", 18.5912, 73.7389),
            ("Baner", 18.5590, 73.7868), ("Kothrud", 18.5074, 73.8077),
            ("Viman Nagar", 18.5679, 73.9143),
        ];
        for (name, lat, lon) in NEIGHBOURHOODS {
            self.insert_authoritative(Place {
                name: (*name).to_string(),
                kind: "suburb".to_string(),
                lat: *lat,
                lon: *lon,
                population: 0,
                country: "in".to_string(),
            });
        }

        for (name, kind, lat, lon) in ADMIN {
            self.insert_authoritative(Place {
                name: (*name).to_string(),
                kind: (*kind).to_string(),
                lat: *lat,
                lon: *lon,
                population: 0,
                country: "in".to_string(),
            });
        }
    }

    /// Insert, replacing any existing entries with the same name.
    ///
    /// Needed because OSM carries a `place=state` node for Delhi, which
    /// outranks a `city` entry and makes "starbucks in delhi" decline as
    /// too broad. Delhi is administratively a state and practically a
    /// metro; for search, the metro reading is the one users mean.
    pub fn insert_authoritative(&mut self, place: Place) {
        let key = normalise(&place.name);
        if key.is_empty() {
            return;
        }
        self.by_name.insert(key, vec![place]);
    }

    pub fn insert(&mut self, place: Place) {
        let key = normalise(&place.name);
        if key.is_empty() {
            return;
        }
        self.by_name.entry(key).or_default().push(place);
    }

    pub fn len(&self) -> usize {
        self.by_name.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Best match for a place name, or None.
    ///
    /// `near` biases selection toward the user's own location, which is what
    /// makes bare names behave sensibly: someone in Bengaluru typing
    /// "Indiranagar" means the one down the road, not the same-named
    /// locality several states away.
    pub fn lookup(&self, name: &str, near: Option<(f64, f64)>) -> Option<&Place> {
        let candidates = self.by_name.get(&normalise(name))?;
        if candidates.len() == 1 {
            return candidates.first();
        }

        candidates.iter().max_by(|a, b| {
            let score = |p: &Place| -> f64 {
                let mut s = p.rank() as f64 * 1000.0;
                // Population is a weak, sparsely-populated tag; it only ever
                // breaks ties within a settlement class.
                s += (p.population as f64).sqrt();
                if let Some((lat, lon)) = near {
                    // Strong proximity bonus, decaying over ~50 km.
                    let d = crate::geo::haversine_m(lat, lon, p.lat, p.lon);
                    s += 4000.0 * (-d / 50_000.0).exp();
                }
                s
            };
            score(a).partial_cmp(&score(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    pub fn save(&self, path: &str) -> anyhow::Result<()> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }

    pub fn load(path: &str) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

/// Split a query into (subject, place) on an " in " separator.
///
/// Returns None when there is no place clause, so the caller falls back to
/// coordinate-based search. The subject may be empty — "in Koramangala"
/// alone is a legitimate "show me what's there" query.
pub fn split_in_clause(query: &str) -> Option<(String, String)> {
    let trimmed = query.trim();
    let lower = trimmed.to_lowercase();

    // A query that *starts* with "in " has no subject, just a place —
    // "in Chennai" means "show me what's in Chennai". Handled explicitly
    // because the general form below looks for a leading space, so this
    // case previously fell through and was searched as literal text,
    // matching place names containing the word "in".
    if let Some(rest) = lower.strip_prefix("in ") {
        let place = trimmed[trimmed.len() - rest.len()..].trim().to_string();
        if !place.is_empty() && !place.contains(" in ") {
            return Some((String::new(), place));
        }
    }

    // Otherwise take the rightmost clause: "shops in mall in andheri"
    // means Andheri.
    let idx = lower.rfind(" in ")?;
    let subject = trimmed[..idx].trim().to_string();
    let place = trimmed[idx + 4..].trim().to_string();
    if place.is_empty() {
        return None;
    }
    Some((subject, place))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, kind: &str, lat: f64, lon: f64, pop: u64) -> Place {
        Place {
            name: name.into(),
            kind: kind.into(),
            lat,
            lon,
            population: pop,
            country: String::new(),
        }
    }

    #[test]
    fn splits_in_clauses() {
        assert_eq!(
            split_in_clause("cafes in koramangala"),
            Some(("cafes".into(), "koramangala".into()))
        );
        assert_eq!(
            split_in_clause("best biryani in Hyderabad"),
            Some(("best biryani".into(), "Hyderabad".into()))
        );
        // No place clause.
        assert_eq!(split_in_clause("cafes near me"), None);
        assert_eq!(split_in_clause("starbucks"), None);
    }

    #[test]
    fn handles_a_query_that_is_only_a_place_clause() {
        // "in chennai" previously fell through and matched place NAMES
        // containing "in" — "Wine In Progress", "One Night in Bangkok".
        assert_eq!(split_in_clause("in chennai"), Some((String::new(), "chennai".into())));
        assert_eq!(split_in_clause("  In Mumbai "), Some((String::new(), "Mumbai".into())));
    }

    #[test]
    fn does_not_split_words_merely_containing_in() {
        // "india" and "information" start with "in" but are not clauses.
        assert_eq!(split_in_clause("india"), None);
        assert_eq!(split_in_clause("information"), None);
        assert_eq!(split_in_clause("indiranagar"), None);
    }

    #[test]
    fn takes_the_rightmost_place_clause() {
        let (subject, place) = split_in_clause("shops in mall in andheri").unwrap();
        assert_eq!(place, "andheri");
        assert_eq!(subject, "shops in mall");
    }

    #[test]
    fn normalisation_makes_user_input_and_osm_names_meet() {
        assert_eq!(normalise("  New   Delhi "), "new delhi");
        assert_eq!(normalise("Bengaluru (Bangalore)"), "bengaluru bangalore");
        assert_eq!(normalise("K.R. Puram"), "k r puram");
    }

    #[test]
    fn bigger_settlement_wins_a_name_collision() {
        let mut g = Gazetteer::default();
        g.insert(p("Rampur", "village", 25.0, 80.0, 500));
        g.insert(p("Rampur", "city", 28.8, 79.0, 300_000));
        assert_eq!(g.lookup("Rampur", None).unwrap().kind, "city");
    }

    /// The behaviour that makes bare place names usable: prefer the one
    /// near the user when the type is the same.
    #[test]
    fn proximity_breaks_ties_between_equal_kinds() {
        let mut g = Gazetteer::default();
        g.insert(p("Indiranagar", "suburb", 12.97, 77.64, 0)); // Bengaluru
        g.insert(p("Indiranagar", "suburb", 17.44, 78.49, 0)); // Hyderabad
        let from_blr = g.lookup("Indiranagar", Some((12.97, 77.59))).unwrap();
        assert!((from_blr.lat - 12.97).abs() < 0.1, "should pick the Bengaluru one");
        let from_hyd = g.lookup("Indiranagar", Some((17.38, 78.48))).unwrap();
        assert!((from_hyd.lat - 17.44).abs() < 0.1, "should pick the Hyderabad one");
    }

    #[test]
    fn radius_scales_with_settlement_size() {
        assert!(p("X", "city", 0.0, 0.0, 0).default_radius_m()
            > p("X", "suburb", 0.0, 0.0, 0).default_radius_m());
        assert!(p("X", "suburb", 0.0, 0.0, 0).default_radius_m()
            > p("X", "neighbourhood", 0.0, 0.0, 0).default_radius_m());
    }

    #[test]
    fn metro_neighbourhoods_beat_same_named_villages() {
        let mut g = Gazetteer::default();
        // What the PBF actually gives us for "bandra".
        g.insert(p("Bandra", "village", 21.64, 79.39, 0));
        g.seed_admin_areas();
        let found = g.lookup("bandra", None).unwrap();
        assert!(
            (found.lat - 19.06).abs() < 0.2,
            "Bandra should resolve to Mumbai, got {},{}",
            found.lat,
            found.lon
        );
    }

    #[test]
    fn former_city_names_resolve() {
        let mut g = Gazetteer::default();
        g.seed_admin_areas();
        for alias in ["bangalore", "madras", "bombay", "calcutta", "gurgaon", "vizag"] {
            assert!(g.lookup(alias, None).is_some(), "{alias} should resolve");
        }
    }

    #[test]
    fn seeded_metros_override_a_state_tagged_node() {
        let mut g = Gazetteer::default();
        // Simulate what the PBF actually contains for Delhi.
        g.insert(p("Delhi", "state", 28.7, 77.1, 0));
        g.seed_admin_areas();
        let found = g.lookup("delhi", None).unwrap();
        assert_eq!(found.kind, "city", "Delhi must resolve as a metro, not a state");
    }

    #[test]
    fn admin_areas_resolve_after_seeding() {
        let mut g = Gazetteer::default();
        g.seed_admin_areas();
        assert!(g.lookup("india", None).is_some(), "country names must resolve");
        assert!(g.lookup("Tamil Nadu", None).is_some(), "state names must resolve");
        assert!(g.lookup("KARNATAKA", None).is_some(), "lookup is case-insensitive");
    }

    #[test]
    fn unknown_place_returns_none_rather_than_guessing() {
        let g = Gazetteer::default();
        assert!(g.lookup("Nowhereville", None).is_none());
    }
}
