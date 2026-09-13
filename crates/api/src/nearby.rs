//! `/nearby` — local search over the places index.
//!
//! Query flow, and why it's in this order:
//!   1. Turn (lat, lon, radius) into a set of geohash cells.
//!   2. Term-match those cells to get candidates cheaply from the inverted
//!      index — no distance maths over the whole corpus.
//!   3. Compute exact haversine distance on candidates and drop anything
//!      outside the true radius (cells are squares, the query is a circle).
//!   4. Rank by proximity + text match + prominence.
//!
//! Step 3 matters: without it you return corners of the bounding box that
//! are up to ~40% further away than the radius the user asked for.

use anyhow::Result;
use axum::extract::{Query as AxumQuery, State};
use axum::Json;
use places::geo;
use places::poi::Poi;
use places::schema::PlaceFields;
use places::search::{self, NearbyResult, NearbyWeights};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Index, IndexReader, TantivyDocument, Term};

pub struct PlacesShard {
    pub index: Index,
    pub fields: PlaceFields,
    pub reader: IndexReader,
    /// Which geohash tiles we already hold, and when they were fetched.
    pub cache: tokio::sync::Mutex<places::cache::TileCache>,
    pub http: reqwest::Client,
    /// Serialises index writes and, critically, prevents two concurrent
    /// requests for the same uncached area from both hitting Overpass.
    pub writer: tokio::sync::Mutex<()>,
    pub ttl_secs: u64,
    /// When false, never call Overpass — serve only what's already indexed.
    pub lazy_fetch: bool,
    /// Place-name -> coordinates, for "X in <place>" queries.
    pub gazetteer: places::gazetteer::Gazetteer,
}

#[derive(Deserialize)]
pub struct NearbyParams {
    /// Optional free text ("cafe", "pharmacy", "dosa"). Omit to get
    /// everything nearby.
    #[serde(default)]
    pub q: String,
    pub lat: f64,
    pub lon: f64,
    /// Search radius in metres.
    #[serde(default = "default_radius")]
    pub radius: f64,
    /// Mirrors `radius` but stays None when the caller omitted it, so a
    /// resolved place can pick its own radius without overriding an
    /// explicit request.
    #[serde(default, rename = "radius")]
    pub radius_given: Option<f64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub explain: bool,
}
fn default_radius() -> f64 {
    2000.0
}
fn default_limit() -> usize {
    20
}

#[derive(Serialize)]
pub struct NearbyResponse {
    pub query: String,
    /// Set when the query named a place ("cafes in Koramangala") and it was
    /// resolved. Lets the client say *where* it searched instead of leaving
    /// the user to guess.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_place: Option<ResolvedPlace>,
    /// The place name the user asked for that could not be resolved.
    ///
    /// Present so the client can say "couldn't find X, showing results near
    /// you" instead of silently answering a different question. Falling back
    /// without telling anyone is how "hospital in india" quietly returned
    /// hospitals in Bengaluru.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_place: Option<String>,
    pub center: [f64; 2],
    pub radius_m: f64,
    /// Radius originally asked for, when the search had to widen to find
    /// anything. Lets the UI say "widened to 5 km" rather than silently
    /// returning results from further away than requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_radius_m: Option<f64>,
    pub took_ms: u64,
    pub total_candidates: usize,
    /// How many tiles had to be fetched from Overpass for this query.
    /// 0 means fully served from the local index.
    pub tiles_fetched: usize,
    pub results: Vec<NearbyResult>,
}

#[derive(Serialize)]
pub struct ResolvedPlace {
    pub name: String,
    pub kind: String,
    pub lat: f64,
    pub lon: f64,
    pub radius_m: f64,
    /// True for country- and state-sized areas.
    ///
    /// Proximity ranking is the wrong model at this scale. "cafes in India"
    /// cannot be answered by distance from a point — the centroid of India
    /// is farmland in Madhya Pradesh, so the honest answer is a handful of
    /// arbitrary rural cafes. Rather than present that as if it meant
    /// something, the client is told the scope is too broad and asked to
    /// name a city. Answering a different question confidently is worse
    /// than declining the one asked.
    pub scope_too_broad: bool,
}

pub fn open_places(dir: &str, lazy_fetch: bool, ttl_secs: u64) -> Result<PlacesShard> {
    let (index, fields) = places::schema::open_index(dir)?;
    let reader = index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let cache = places::cache::TileCache::load(format!("{dir}/tiles.json"));
    let gazetteer = places::gazetteer::Gazetteer::load(&format!("{dir}/gazetteer.json"));
    tracing::info!("gazetteer: {} place names", gazetteer.len());
    let http = reqwest::Client::builder()
        .user_agent(places::fetch::USER_AGENT)
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    Ok(PlacesShard {
        index,
        fields,
        reader,
        cache: tokio::sync::Mutex::new(cache),
        http,
        writer: tokio::sync::Mutex::new(()),
        ttl_secs,
        lazy_fetch,
        gazetteer,
    })
}

/// Ensure every tile covering the query is present and fresh, fetching the
/// missing ones from Overpass and indexing them.
///
/// Returns how many tiles were fetched, so the response can report whether
/// the query was a cache hit — useful when demoing why this design exists.
async fn ensure_tiles(shard: &PlacesShard, lat: f64, lon: f64, radius_m: f64) -> Result<usize> {
    if !shard.lazy_fetch {
        return Ok(0);
    }

    let wanted = places::cache::tiles_for_query(lat, lon, radius_m);
    let missing = {
        let cache = shard.cache.lock().await;
        cache.stale_tiles(&wanted, shard.ttl_secs)
    };
    if missing.is_empty() {
        return Ok(0);
    }

    // Hold the write lock across fetch+index so a second request for the
    // same area waits rather than issuing a duplicate Overpass call. OSM's
    // policy allows very little concurrency, so serialising here is correct
    // even though it costs latency on a cold tile.
    let _guard = shard.writer.lock().await;

    // Re-check: another request may have filled these while we waited.
    let still_missing = {
        let cache = shard.cache.lock().await;
        cache.stale_tiles(&missing, shard.ttl_secs)
    };
    if still_missing.is_empty() {
        return Ok(0);
    }

    // Safety valve: a very large radius over cold ground could otherwise
    // issue dozens of Overpass calls in one request. Fetch a bounded number
    // and let subsequent queries fill the rest.
    const MAX_TILES_PER_REQUEST: usize = 4;
    let to_fetch: Vec<String> =
        still_missing.iter().take(MAX_TILES_PER_REQUEST).cloned().collect();
    if still_missing.len() > MAX_TILES_PER_REQUEST {
        tracing::info!(
            "{} tiles missing; fetching {} this request",
            still_missing.len(),
            MAX_TILES_PER_REQUEST
        );
    }

    let mut fetched = 0;
    let mut all_pois = Vec::new();
    for tile in &to_fetch {
        let Some((tlat, tlon, tradius)) = places::cache::tile_bounds(tile) else {
            continue;
        };
        tracing::info!("cache miss for tile {tile}; fetching from Overpass");
        match places::fetch::fetch_area(&shard.http, tlat, tlon, tradius).await {
            Ok(pois) => {
                tracing::info!("tile {tile}: {} places", pois.len());
                all_pois.extend(pois);
                fetched += 1;
            }
            Err(e) => {
                // A failed fetch must not poison the cache, or the tile
                // would look populated while holding nothing.
                tracing::warn!("tile {tile} fetch failed: {e:#}");
                continue;
            }
        }
    }

    if !all_pois.is_empty() {
        let index = shard.index.clone();
        let fields = shard.fields.clone();
        tokio::task::spawn_blocking(move || places::schema::add_places(&index, &fields, &all_pois))
            .await
            .map_err(|e| anyhow::anyhow!("index write panicked: {e}"))??;

        // Force the reader to pick up the commit now. The default reload
        // policy is delayed, so without this the very query that triggered
        // the fetch returns nothing — the worst possible first impression.
        shard.reader.reload()?;
    }

    // Only mark tiles we actually fetched successfully.
    if fetched > 0 {
        let mut cache = shard.cache.lock().await;
        for tile in to_fetch.iter().take(fetched) {
            cache.mark_fetched(tile);
        }
        if let Err(e) = cache.save() {
            tracing::warn!("could not persist tile cache: {e:#}");
        }
    }
    Ok(fetched)
}

/// Place-index size, for the same honesty reason as `/stats`.
pub async fn places_stats_handler(State(shard): State<Arc<PlacesShard>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "places": shard.reader.searcher().num_docs(),
        "place_names": shard.gazetteer.len(),
    }))
}

pub async fn nearby_handler(
    State(shard): State<Arc<PlacesShard>>,
    AxumQuery(p): AxumQuery<NearbyParams>,
) -> Result<Json<NearbyResponse>, crate::AppError> {
    let start = std::time::Instant::now();

    // Guard the inputs: a bad lat/lon silently returns nothing otherwise,
    // which is indistinguishable from "no places here".
    if !p.lat.is_finite() || !p.lon.is_finite() {
        return Err(crate::AppError::bad_request("lat and lon must be finite numbers"));
    }
    if !(-90.0..=90.0).contains(&p.lat) || !(-180.0..=180.0).contains(&p.lon) {
        return Err(crate::AppError::bad_request(format!(
            "lat must be within -90..90 and lon within -180..180 (got {}, {})",
            p.lat, p.lon
        )));
    }
    let mut radius = p.radius.clamp(50.0, 50_000.0);
    let limit = p.limit.clamp(1, 100);

    // "X in <place>" overrides the caller's coordinates. Resolving this
    // first matters: without it the place clause was silently ignored and
    // results came from wherever the client happened to be — a wrong answer
    // that looks like a right one.
    let mut lat = p.lat;
    let mut lon = p.lon;
    let mut subject = p.q.clone();
    let mut resolved_place = None;
    let mut unresolved_place = None;

    if let Some((subj, place_name)) = places::gazetteer::split_in_clause(&p.q) {
        if let Some(place) = shard.gazetteer.lookup(&place_name, Some((p.lat, p.lon))) {
            lat = place.lat;
            lon = place.lon;
            // Radius follows the settlement size unless explicitly given.
            if p.radius_given.is_none() {
                radius = place.default_radius_m();
            }
            subject = subj;
            let too_broad = matches!(place.kind.as_str(), "country" | "state");
            resolved_place = Some(ResolvedPlace {
                name: place.name.clone(),
                kind: place.kind.clone(),
                lat: place.lat,
                lon: place.lon,
                radius_m: radius,
                scope_too_broad: too_broad,
            });
        }
        else {
            // Fall through to coordinate search rather than returning
            // nothing, but record what we failed to resolve.
            unresolved_place = Some(place_name);
        }
    } else if !places::search::has_local_intent(&p.q) {
        // No " in " clause, but the query may still name a city inline:
        // "IIM Bangalore", "Phoenix Mall Mumbai". Without this the lookup
        // stayed pinned to the caller's coordinates, so a national landmark
        // searched from another state returned nothing at all.
        //
        // Skipped when the query says "near me" — there the user's own
        // position is the whole point, even if a city name appears.
        if let Some((place, token)) = find_embedded_place(&shard.gazetteer, &p.q, p.lat, p.lon) {
            lat = place.lat;
            lon = place.lon;
            if p.radius_given.is_none() {
                radius = place.default_radius_m();
            }
            let too_broad = matches!(place.kind.as_str(), "country" | "state");
            resolved_place = Some(ResolvedPlace {
                name: place.name.clone(),
                kind: place.kind.clone(),
                lat: place.lat,
                lon: place.lon,
                radius_m: radius,
                scope_too_broad: too_broad,
            });
            // The city name is context, not part of what is being searched
            // for; leaving it in makes every place in that city a partial
            // match on the city's own name.
            subject = p
                .q
                .split_whitespace()
                .filter(|w| !w.eq_ignore_ascii_case(&token))
                .collect::<Vec<_>>()
                .join(" ");
        }
    }

    // Fill any missing tiles before querying. On a warm area this is a
    // no-op; on a cold one it costs one Overpass round-trip, once.
    let tiles_fetched = ensure_tiles(&shard, lat, lon, radius).await?;

    let q = subject;
    let explain = p.explain;
    // Widen the search when the immediate area is thin.
    //
    // OSM coverage varies enormously across India: 2 km around a Bengaluru
    // junction holds hundreds of places, while the same radius in a smaller
    // city can hold none. Returning "nothing found" there is misleading —
    // the places exist, just slightly further out. Measured in Ludhiana:
    // "atm" gave 0 results at 2 km and 13 at 5 km.
    const MIN_USEFUL_RESULTS: usize = 3;
    let requested = radius;
    let mut results = Vec::new();
    let mut candidates = 0usize;
    let mut used_radius = radius;

    for factor in [1.0, 2.5, 5.0] {
        let r = (requested * factor).min(50_000.0);
        let shard = shard.clone();
        let q = q.clone();
        let (res, cand) =
            tokio::task::spawn_blocking(move || run_nearby(&shard, &q, lat, lon, r, limit, explain))
                .await
                .map_err(|e| anyhow::anyhow!("nearby task panicked: {e}"))??;
        used_radius = r;
        results = res;
        candidates = cand;
        if results.len() >= MIN_USEFUL_RESULTS || r >= 50_000.0 {
            break;
        }
    }
    let widened = used_radius > requested && !results.is_empty();
    radius = used_radius;

    Ok(Json(NearbyResponse {
        query: p.q,
        resolved_place,
        unresolved_place,
        center: [lat, lon],
        radius_m: radius,
        requested_radius_m: widened.then_some(requested),
        took_ms: start.elapsed().as_millis() as u64,
        total_candidates: candidates,
        tiles_fetched,
        results,
    }))
}

/// Find a city name embedded in a query, e.g. the "Bangalore" in
/// "IIM Bangalore".
///
/// Only trailing-ish tokens are considered and category words are skipped,
/// so "Church Street" is not read as a place called "church". Returns the
/// resolved place and the token it came from, so the caller can strip it.
fn find_embedded_place<'a>(
    gazetteer: &'a places::gazetteer::Gazetteer,
    query: &str,
    lat: f64,
    lon: f64,
) -> Option<(&'a places::gazetteer::Place, String)> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.len() < 2 {
        return None; // a bare name is not a location query
    }
    // Scan from the end: "IIM Bangalore" puts the city last, and so does
    // almost every natural phrasing of this shape.
    for w in words.iter().rev() {
        let token = w.trim_matches(|c: char| !c.is_alphanumeric());
        if token.len() < 4 || places::synonyms::is_category_word(token) {
            continue;
        }
        if let Some(place) = gazetteer.lookup(token, Some((lat, lon))) {
            // Only settlement-scale names; a village match would pin a
            // national query to somewhere arbitrary.
            if matches!(place.kind.as_str(), "city" | "town" | "suburb" | "state" | "country") {
                return Some((place, token.to_string()));
            }
        }
    }
    None
}

fn run_nearby(
    shard: &PlacesShard,
    q: &str,
    lat: f64,
    lon: f64,
    radius: f64,
    limit: usize,
    explain: bool,
) -> Result<(Vec<NearbyResult>, usize)> {
    let f = &shard.fields;
    let searcher = shard.reader.searcher();

    // 1. Geohash cells covering the circle -> cheap candidate retrieval.
    let cells = geo::cells_covering(lat, lon, radius);
    let cell_clauses: Vec<(Occur, Box<dyn Query>)> = cells
        .iter()
        .map(|c| {
            let term = Term::from_field_text(f.geocell, c);
            (
                Occur::Should,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
            )
        })
        .collect();
    let geo_query: Box<dyn Query> = Box::new(BooleanQuery::new(cell_clauses));

    // 2. Optional text constraint, ANDed with the geo constraint.
    // Query understanding: strip locality markers, then expand colloquial
    // terms into the OSM vocabulary ("petrol" -> fuel, "doctor" -> doctors).
    // Without this, common queries silently return nothing.
    let stripped = search::strip_local_markers(q);
    let text = if stripped.is_empty() {
        String::new()
    } else {
        places::synonyms::expand(&stripped).join(" OR ")
    };
    let query: Box<dyn Query> = if text.is_empty() {
        geo_query
    } else {
        let mut parser =
            QueryParser::for_index(&shard.index, vec![f.name, f.brand, f.category, f.group]);
        parser.set_field_boost(f.name, 2.0);
        parser.set_field_boost(f.brand, 2.0);
        match parser.parse_query(&text) {
            Ok(tq) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, geo_query),
                (Occur::Must, tq),
            ])),
            // Unparseable text shouldn't turn into "no results" — fall back
            // to plain proximity, which is still a useful answer.
            Err(_) => geo_query,
        }
    };

    // Over-fetch: the index can't sort by distance, so take a generous slice
    // and let exact distance ranking pick the winners.
    let fetch = (limit * 40).clamp(200, 3000);
    let hits = searcher.search(&query, &TopDocs::with_limit(fetch))?;
    let candidates = hits.len();

    let max_text = hits.iter().map(|(s, _)| *s).fold(0.0f32, f32::max);
    let weights = NearbyWeights::default();

    let mut out = Vec::new();
    for (text_score, addr) in hits {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let (Some(plat), Some(plon)) = (
            doc.get_first(f.lat).and_then(|v| v.as_f64()),
            doc.get_first(f.lon).and_then(|v| v.as_f64()),
        ) else {
            continue;
        };

        // 3. Exact distance — the cells are squares, the query is a circle.
        let distance_m = geo::haversine_m(lat, lon, plat, plon);
        if distance_m > radius {
            continue;
        }

        let prominence = doc.get_first(f.prominence).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let norm_text = if max_text > 0.0 { text_score / max_text } else { 0.0 };
        let (score, ex) = search::score(norm_text, distance_m, prominence, radius, &weights);

        out.push(NearbyResult {
            poi: Poi {
                id: s(&doc, f.id),
                name: s(&doc, f.name),
                brand: s(&doc, f.brand),
                category: s(&doc, f.category),
                group: s(&doc, f.group),
                lat: plat,
                lon: plon,
                address: s(&doc, f.address),
                phone: s(&doc, f.phone),
                website: s(&doc, f.website),
                opening_hours: s(&doc, f.opening_hours),
                prominence,
            },
            distance_m,
            score,
            explain: explain.then_some(ex),
        });
    }

    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Poor-match suppression. When the query names a specific place and
    // nothing matches its distinctive core, return nothing rather than the
    // nearest thing that happens to share a city name.
    let is_place_name = |w: &str| shard.gazetteer.lookup(w, None).is_some();
    let distinctive = search::distinctive_terms(&stripped, &is_place_name);
    if !distinctive.is_empty() {
        out.retain(|r| search::distinctive_match_ok(&r.poi.name, &r.poi.brand, &distinctive));
    }

    out.truncate(limit);
    Ok((out, candidates))
}

fn s(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
    doc.get_first(field).and_then(|v| v.as_str()).unwrap_or("").to_string()
}
