//! Query understanding for local search: mapping what people type to what
//! OpenStreetMap actually calls things.
//!
//! This layer exists because there are two vocabularies and they don't
//! match. OSM uses a controlled tag set (`amenity=doctors`, `amenity=fuel`);
//! people type what they say out loud ("doctor", "petrol pump"). Lexical
//! matching across that gap fails silently — measured before this module
//! existed, `doctor` returned 0 results and `petrol` returned 0, while the
//! data held hundreds of each.
//!
//! Indian-English terms are first-class here, not an afterthought: "chemist",
//! "medical store", "petrol pump" and "kirana" are what users in this market
//! actually type, and none of them appear anywhere in OSM's tag vocabulary.

/// Concept -> OSM category/group terms. The query keeps its original words
/// *and* gains these, so a literal name match ("Apollo Pharmacy") still wins
/// on its own merits while the concept broadens recall.
const CONCEPTS: &[(&str, &[&str])] = &[
    // Food and drink
    ("coffee",        &["cafe", "coffee_shop"]),
    ("cafe",          &["cafe"]),
    ("tea",           &["cafe", "tea_house"]),
    ("chai",          &["cafe", "tea_house"]),
    ("food",          &["restaurant", "fast_food", "cafe", "food_court"]),
    ("eat",           &["restaurant", "fast_food", "food_court"]),
    ("hungry",        &["restaurant", "fast_food", "food_court"]),
    ("lunch",         &["restaurant", "fast_food", "food_court"]),
    ("dinner",        &["restaurant", "bar", "pub"]),
    ("breakfast",     &["cafe", "restaurant", "bakery"]),
    ("drinks",        &["bar", "pub", "biergarten"]),
    ("bakery",        &["bakery", "pastry"]),
    ("sweets",        &["confectionery", "bakery"]),

    // Health — the category most costly to get wrong
    ("doctor",        &["doctors", "clinic", "hospital"]),
    ("physician",     &["doctors", "clinic"]),
    ("medical",       &["pharmacy", "clinic", "doctors", "hospital"]),
    ("medicine",      &["pharmacy", "chemist"]),
    ("chemist",       &["pharmacy"]),          // Indian English
    ("druggist",      &["pharmacy"]),
    ("drugstore",     &["pharmacy"]),
    ("emergency",     &["hospital", "clinic"]),
    ("dentist",       &["dentist"]),
    ("vet",           &["veterinary"]),
    ("lab",           &["laboratory", "clinic"]),

    // Money
    ("cash",          &["atm", "bank"]),
    ("money",         &["atm", "bank", "bureau_de_change"]),
    ("atm",           &["atm"]),
    ("exchange",      &["bureau_de_change", "bank"]),

    // Vehicle — "petrol pump" is the Indian term; OSM says `fuel`
    ("petrol",        &["fuel"]),
    ("diesel",        &["fuel"]),
    ("gas",           &["fuel"]),
    ("fuel",          &["fuel"]),
    ("charging",      &["charging_station"]),
    ("ev",            &["charging_station"]),
    ("mechanic",      &["car_repair"]),
    ("repair",        &["car_repair", "electronics_repair"]),
    ("garage",        &["car_repair"]),
    ("stationery",    &["stationery", "books"]),
    ("laundry",       &["laundry", "dry_cleaning"]),
    ("optician",      &["optician"]),
    ("jeweller",      &["jewelry"]),
    ("florist",       &["florist"]),
    ("hardware",      &["hardware", "doityourself"]),
    ("puncture",      &["car_repair", "tyres"]),
    ("parking",       &["parking"]),

    // Shopping
    ("grocery",       &["supermarket", "convenience", "greengrocer"]),
    ("groceries",     &["supermarket", "convenience", "greengrocer"]),
    ("kirana",        &["convenience", "supermarket"]),   // Indian English
    ("supermarket",   &["supermarket"]),
    ("mall",          &["mall", "department_store"]),
    ("shopping",      &["mall", "department_store", "supermarket"]),
    ("clothes",       &["clothes", "boutique"]),
    ("salon",         &["hairdresser", "beauty"]),
    ("barber",        &["hairdresser"]),

    // Stay and transport
    ("hotel",         &["hotel", "guest_house"]),
    ("stay",          &["hotel", "guest_house", "hostel"]),
    ("lodge",         &["hotel", "guest_house", "hostel"]),
    ("bus",           &["bus_station", "bus_stop"]),
    ("train",         &["railway_station", "station"]),
    ("metro",         &["subway", "station"]),
    ("airport",       &["airport", "aerodrome"]),
    ("taxi",          &["taxi"]),

    // Civic, education, leisure
    ("police",        &["police"]),
    ("post",          &["post_office"]),
    ("school",        &["school"]),
    ("college",       &["college", "university"]),
    ("library",       &["library"]),
    ("gym",           &["fitness_centre", "gym", "sports_centre"]),
    ("fitness",       &["fitness_centre", "gym"]),
    ("workout",       &["fitness_centre", "gym"]),
    ("park",          &["park", "garden"]),
    ("temple",        &["place_of_worship", "temple"]),
    ("church",        &["place_of_worship", "church"]),
    ("mosque",        &["place_of_worship", "mosque"]),
    ("gurudwara",     &["place_of_worship"]),
    ("worship",       &["place_of_worship"]),
    ("movie",         &["cinema"]),
    ("cinema",        &["cinema"]),
    ("museum",        &["museum"]),
];

/// Singularise a token well enough for concept lookup.
///
/// Note this is only for matching the concept table — the index itself is
/// stemmed by tantivy, so this does not need to be a general stemmer. It
/// just has to turn "cafes" into "cafe" before the table lookup.
fn singular(word: &str) -> String {
    if word.len() > 3 {
        // "pharmacies" -> "pharmacy"
        if let Some(stem) = word.strip_suffix("ies") {
            return format!("{stem}y");
        }
        // "churches" -> "church": strip only the "es", not the whole
        // consonant cluster. Stripping "ches" wholesale yields "chur".
        for ending in ["ches", "shes", "ses", "xes", "zes"] {
            if word.ends_with(ending) {
                return word[..word.len() - 2].to_string();
            }
        }
        if let Some(stem) = word.strip_suffix('s') {
            if !stem.ends_with('s') {
                return stem.to_string();
            }
        }
    }
    word.to_string()
}

/// Multi-word category phrases whose parts mean different things alone.
/// "petrol pump" is fuel; "pump" by itself identifies nothing.
pub fn phrase_concept(phrase: &str) -> Option<&'static [&'static str]> {
    match phrase {
        "petrol pump" | "gas station" | "filling station" | "petrol station" => Some(&["fuel"]),
        "medical store" | "medical shop" => Some(&["pharmacy"]),
        "coffee shop" => Some(&["cafe"]),
        "car repair" | "car mechanic" | "auto repair" => Some(&["car_repair"]),
        "dry cleaner" | "dry cleaning" => Some(&["laundry", "dry_cleaning"]),
        "bus stand" | "bus stop" => Some(&["bus_station", "bus_stop"]),
        "railway station" | "train station" => Some(&["railway_station", "station"]),
        "police station" => Some(&["police"]),
        "post office" => Some(&["post_office"]),
        "departmental store" | "department store" => Some(&["department_store", "supermarket"]),
        _ => None,
    }
}

/// Is this word a *kind* of place rather than the name of one?
///
/// Two sources, because neither alone is complete: the concept table covers
/// colloquial words ("chemist", "petrol"), and `group_for` covers raw OSM
/// category values ("hospital", "cafe", "college") which never appear as
/// concept keys because they need no translation.
///
/// Checking "did expansion add terms?" instead — the first attempt — fails
/// for concepts that map to themselves: expand("cafe") returns just
/// ["cafe"], so "cafe" looked like a distinctive name.
pub fn is_category_word(word: &str) -> bool {
    let key = singular(&word.to_lowercase());
    if CONCEPTS.iter().any(|(c, _)| *c == key) {
        return true;
    }
    crate::poi::group_for(&key) != "other"
}

/// Expand a query with OSM category terms implied by its words.
///
/// Returns the original terms plus any expansions, deduplicated. The
/// original is always kept so a literal name search still behaves: someone
/// typing "Apollo" wants the place called Apollo, not every pharmacy.
pub fn expand(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        let t = t.to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    };

    let words: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect();

    for w in &words {
        push(w);
    }

    // Two-word phrases first — "petrol pump" and "medical store" are single
    // concepts whose parts mean different things alone.
    for pair in words.windows(2) {
        if let Some(terms) = phrase_concept(&pair.join(" ")) {
            for t in terms {
                push(t);
            }
        }
    }

    for w in &words {
        let key = singular(w);
        // Always contribute the singular form, concept or not. The index is
        // stemmed, but the concept table is keyed on singulars, and a plural
        // with no concept entry ("hospitals") would otherwise expand to
        // nothing at all.
        if key != *w {
            push(&key);
        }
        if let Some((_, terms)) = CONCEPTS.iter().find(|(c, _)| *c == key) {
            for t in *terms {
                push(t);
            }
        }
    }

    out
}

/// Categories worth offering as one-tap suggestions once a location is known.
/// Ordered by how often people actually search them.
pub const SUGGESTED: &[(&str, &str)] = &[
    ("☕", "cafe"),
    ("🍽", "restaurant"),
    ("💊", "pharmacy"),
    ("🏥", "hospital"),
    ("🏧", "atm"),
    ("⛽", "petrol"),
    ("🛒", "grocery"),
    ("🏨", "hotel"),
    ("🏋", "gym"),
    ("🅿️", "parking"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn has(query: &str, term: &str) -> bool {
        expand(query).iter().any(|t| t == term)
    }

    #[test]
    fn plurals_reach_the_concept_table() {
        assert!(has("cafes", "cafe"));
        assert!(has("hospitals", "hospital"));
        assert!(has("groceries", "supermarket"));
        assert!(has("pharmacies", "pharmacy"));
    }

    /// Each of these returned zero results before this module existed.
    #[test]
    fn colloquial_terms_map_to_osm_categories() {
        assert!(has("doctor", "doctors"), "doctor -> amenity=doctors");
        assert!(has("petrol", "fuel"), "petrol -> amenity=fuel");
        assert!(has("medicine", "pharmacy"));
        assert!(has("coffee", "cafe"));
        assert!(has("cash", "atm"));
        assert!(has("gym", "fitness_centre"));
    }

    #[test]
    fn indian_english_is_supported() {
        assert!(has("chemist", "pharmacy"));
        assert!(has("kirana", "convenience"));
        assert!(has("petrol pump", "fuel"));
        assert!(has("medical store", "pharmacy"));
    }

    #[test]
    fn multiword_phrases_beat_their_parts() {
        // "gas station" is fuel, even though "station" alone suggests rail.
        assert!(has("gas station", "fuel"));
        assert!(has("railway station", "railway_station"));
        assert!(has("police station", "police"));
    }

    #[test]
    fn original_terms_are_always_preserved() {
        // A branded search must not be swallowed by concept expansion.
        let terms = expand("apollo pharmacy");
        assert!(terms.contains(&"apollo".to_string()));
        assert!(terms.contains(&"pharmacy".to_string()));
    }

    #[test]
    fn unknown_words_pass_through_unchanged() {
        assert_eq!(expand("zomato"), vec!["zomato".to_string()]);
    }

    #[test]
    fn broad_intent_fans_out_to_several_categories() {
        let food = expand("food");
        for want in ["restaurant", "fast_food", "cafe"] {
            assert!(food.contains(&want.to_string()), "food should include {want}");
        }
    }

    #[test]
    fn category_words_are_recognised_from_both_sources() {
        // Concept-table entries.
        assert!(is_category_word("chemist"));
        assert!(is_category_word("petrol"));
        // Raw OSM category values, which are not concept keys.
        assert!(is_category_word("hospital"));
        assert!(is_category_word("cafe"));
        assert!(is_category_word("college"));
        assert!(is_category_word("mall"));
        // Plurals.
        assert!(is_category_word("cafes"));
        assert!(is_category_word("hospitals"));
        // Actual names must not be mistaken for categories.
        assert!(!is_category_word("apollo"));
        assert!(!is_category_word("iim"));
        assert!(!is_category_word("phoenix"));
        assert!(!is_category_word("christian"));
    }

    #[test]
    fn singularise_handles_common_endings() {
        assert_eq!(singular("cafes"), "cafe");
        assert_eq!(singular("pharmacies"), "pharmacy");
        assert_eq!(singular("churches"), "church");
        assert_eq!(singular("bus"), "bus"); // not "bu"
    }
}
