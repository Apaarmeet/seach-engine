use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use places::geo;
use places::poi::{self, Poi};
use places::schema::build_schema;
use tantivy::doc;

/// Build the places index from OpenStreetMap.
#[derive(Parser, Debug)]
#[command(about = "Ingest OSM points of interest into a searchable places index")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Pull a single city from the Overpass API. Small and fast — use this
    /// to iterate before committing to a 1.7 GB country extract.
    City {
        /// Centre latitude.
        #[arg(long)]
        lat: f64,
        /// Centre longitude.
        #[arg(long)]
        lon: f64,
        /// Radius in metres.
        #[arg(long, default_value_t = 5000.0)]
        radius: f64,
        #[arg(long, default_value = "places-index")]
        index_dir: String,
    },
    /// Pre-seed many cities from a JSON list. Warms tiles before users hit
    /// them, so nobody eats a cold-fetch latency.
    Seed {
        /// JSON array of {name, lat, lon, radius_km}.
        #[arg(long, default_value = "crates/places/cities-india.json")]
        cities: String,
        #[arg(long, default_value = "places-index")]
        index_dir: String,
        /// Report the work required and exit without calling Overpass.
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait between tile fetches. OSM allows 2 concurrent
        /// slots; going faster is how you get blocked.
        #[arg(long, default_value_t = 2)]
        delay_secs: u64,
        /// Stop after this many tiles (0 = no limit).
        #[arg(long, default_value_t = 0)]
        max_tiles: usize,
    },
    /// Ingest a whole `.osm.pbf` extract (e.g. india-latest.osm.pbf).
    Pbf {
        #[arg(long)]
        file: String,
        #[arg(long, default_value = "places-index")]
        index_dir: String,
    },
}

/// Shortest geohash prefix indexed per place. Nothing coarser than this is
/// useful for "near me" — a 2-character cell spans most of a subcontinent.
const MIN_PREFIX: usize = 3;
const MAX_PREFIX: usize = 9;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let (pois, index_dir) = match args.cmd {
        Cmd::City { lat, lon, radius, index_dir } => {
            (fetch_overpass(lat, lon, radius)?, index_dir)
        }
        Cmd::Seed { cities, index_dir, dry_run, delay_secs, max_tiles } => {
            return seed_cities(&cities, &index_dir, dry_run, delay_secs, max_tiles);
        }
        Cmd::Pbf { file, index_dir } => return ingest_pbf(&file, &index_dir),
    };

    if pois.is_empty() {
        anyhow::bail!("no POIs found — nothing to index");
    }
    write_index(&pois, &index_dir)?;
    Ok(())
}

fn write_index(pois: &[Poi], index_dir: &str) -> Result<()> {
    let (index, f) = places::schema::open_index(index_dir)?;
    let mut writer = index.writer(100_000_000)?;
    writer.delete_all_documents()?;

    for p in pois {
        let hash = geo::encode(p.lat, p.lon, MAX_PREFIX);
        // Index every prefix so any query precision hits a term directly.
        let cells = geo::prefixes(&hash, MIN_PREFIX).join(" ");
        writer.add_document(doc!(
            f.id            => p.id.clone(),
            f.name          => p.name.clone(),
            f.category      => p.category.clone(),
            f.group         => p.group.clone(),
            f.geocell       => cells,
            f.lat           => p.lat,
            f.lon           => p.lon,
            f.prominence    => poi::prominence(p) as f64,
            f.address       => p.address.clone(),
            f.phone         => p.phone.clone(),
            f.website       => p.website.clone(),
            f.opening_hours => p.opening_hours.clone(),
        ))?;
    }
    writer.commit()?;
    tracing::info!("indexed {} places into {index_dir}", pois.len());
    Ok(())
}

/// Fetch POIs for a small area from the Overpass API.
fn fetch_overpass(lat: f64, lon: f64, radius: f64) -> Result<Vec<Poi>> {
    let query = format!(
        r#"[out:json][timeout:90];
(
  node(around:{radius},{lat},{lon})["amenity"]["name"];
  node(around:{radius},{lat},{lon})["shop"]["name"];
  node(around:{radius},{lat},{lon})["tourism"]["name"];
  node(around:{radius},{lat},{lon})["leisure"]["name"];
);
out body {limit};"#,
        radius = radius as i64,
        limit = 20000
    );

    tracing::info!("querying Overpass for {radius}m around ({lat}, {lon})");
    let client = reqwest::blocking::Client::builder()
        .user_agent("rsearch-places/0.1")
        .timeout(std::time::Duration::from_secs(180))
        .build()?;
    let resp = client
        .post("https://overpass-api.de/api/interpreter")
        .body(query)
        .send()
        .context("overpass request")?
        .error_for_status()
        .context("overpass returned an error status")?;

    let json: serde_json::Value = resp.json()?;
    let elements = json["elements"].as_array().cloned().unwrap_or_default();
    tracing::info!("overpass returned {} elements", elements.len());

    let mut out = Vec::new();
    for el in elements {
        let (Some(lat), Some(lon)) = (el["lat"].as_f64(), el["lon"].as_f64()) else {
            continue;
        };
        let tags = &el["tags"];
        let name = tags["name"].as_str().or_else(|| tags["brand"].as_str());
        let Some(name) = name else { continue };

        let pairs: Vec<(&str, &str)> = tags
            .as_object()
            .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s))).collect())
            .unwrap_or_default();
        let Some((category, group)) = poi::categorise(pairs) else { continue };

        out.push(Poi {
            id: format!("n{}", el["id"].as_i64().unwrap_or(0)),
            name: name.to_string(),
            brand: str_tag(tags, &["brand", "operator"]),
            category,
            group: group.to_string(),
            lat,
            lon,
            address: build_address(tags),
            phone: str_tag(tags, &["phone", "contact:phone"]),
            website: str_tag(tags, &["website", "contact:website"]),
            opening_hours: str_tag(tags, &["opening_hours"]),
            prominence: 0.0,
        });
    }
    tracing::info!("extracted {} named places", out.len());
    Ok(out)
}

/// Stream a `.osm.pbf` extract straight into the index.
///
/// Streaming rather than collecting: India has millions of tagged nodes, and
/// building a Vec of every POI before indexing would hold roughly a gigabyte
/// of strings live at once. Batching keeps peak memory flat regardless of
/// extract size.
///
/// Only nodes are read. Ways and relations would need geometry resolution to
/// derive a centroid — a meaningful amount of extra machinery for a modest
/// coverage gain, since most searchable POIs in OSM are tagged as nodes.
fn ingest_pbf(path: &str, index_dir: &str) -> Result<()> {
    use osmpbf::{Element, ElementReader};

    const BATCH: usize = 50_000;

    let (index, f) = places::schema::open_index(index_dir)?;
    let mut writer = index.writer(300_000_000)?;
    writer.delete_all_documents()?;

    // Every tile we actually put places into. Recorded so the API's lazy
    // fetch knows this ground is already covered — without this the tile
    // cache stays empty and queries re-fetch from Overpass data we already
    // hold locally.
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Place names are collected in the same pass as POIs. A second scan over
    // 291M elements just to find 312k place nodes would be wasteful.
    let mut gazetteer = places::gazetteer::Gazetteer::default();

    // Bounding box of everything seen, so the whole extract can be marked
    // covered in one region rather than tile-by-tile.
    let (mut min_lat, mut max_lat) = (f64::MAX, f64::MIN);
    let (mut min_lon, mut max_lon) = (f64::MAX, f64::MIN);

    let reader = ElementReader::from_path(path).with_context(|| format!("opening {path}"))?;
    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut batch: Vec<Poi> = Vec::with_capacity(BATCH);

    reader.for_each(|element| {
        scanned += 1;
        if scanned % 5_000_000 == 0 {
            tracing::info!("scanned {scanned}M elements, {indexed} places so far", scanned = scanned / 1_000_000);
        }

        let (id, lat, lon, tags): (i64, f64, f64, Vec<(String, String)>) = match element {
            Element::Node(n) => (
                n.id(),
                n.lat(),
                n.lon(),
                n.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ),
            Element::DenseNode(n) => (
                n.id(),
                n.lat(),
                n.lon(),
                n.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ),
            _ => return,
        };

        let name = tags
            .iter()
            .find(|(k, _)| k == "name")
            .or_else(|| tags.iter().find(|(k, _)| k == "brand"))
            .map(|(_, v)| v.clone());
        let Some(name) = name else { return };

        // A `place=*` node is a settlement, not a POI: record it for
        // geocoding and move on.
        if let Some((_, kind)) = tags.iter().find(|(k, _)| k == "place") {
            gazetteer.insert(places::gazetteer::Place {
                name: name.clone(),
                kind: kind.clone(),
                lat,
                lon,
                population: tags
                    .iter()
                    .find(|(k, _)| k == "population")
                    .and_then(|(_, v)| v.replace(',', "").parse().ok())
                    .unwrap_or(0),
            });
            return;
        }
        let pairs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let Some((category, group)) = poi::categorise(pairs) else { return };

        let get = |keys: &[&str]| -> String {
            keys.iter()
                .find_map(|k| tags.iter().find(|(tk, _)| tk == k).map(|(_, v)| v.clone()))
                .unwrap_or_default()
        };

        covered.insert(places::geo::encode(lat, lon, places::cache::TILE_PRECISION));
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);

        batch.push(Poi {
            id: format!("n{id}"),
            name,
            brand: get(&["brand", "operator"]),
            category,
            group: group.to_string(),
            lat,
            lon,
            address: [get(&["addr:housenumber"]), get(&["addr:street"]), get(&["addr:city"])]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            phone: get(&["phone", "contact:phone"]),
            website: get(&["website", "contact:website"]),
            opening_hours: get(&["opening_hours"]),
            prominence: 0.0,
        });
        indexed += 1;

        if batch.len() >= BATCH {
            if let Err(e) = places::schema::write_batch(&mut writer, &f, &batch, false) {
                tracing::error!("batch write failed: {e:#}");
            }
            batch.clear();
        }
    })?;

    if !batch.is_empty() {
        places::schema::write_batch(&mut writer, &f, &batch, false)?;
    }

    // ---- Ways (building polygons) ----
    //
    // A quarter of named, searchable places in OSM are mapped as closed ways
    // rather than points — malls, hospitals, campuses, stadiums. They were
    // skipped entirely until now, which is why "IIM Bangalore" returned a
    // food stall.
    //
    // Ways store node *references*, not coordinates, so this needs a second
    // scan: collect the interesting ways and the ids they reference, then
    // sweep the nodes again to resolve those ids into positions.
    let way_pois = resolve_ways(path, &node_positions_needed(path)?)?;
    tracing::info!("resolved {} polygon places", way_pois.len());

    for chunk in way_pois.chunks(BATCH) {
        places::schema::write_batch(&mut writer, &f, chunk, false)?;
    }
    for p in &way_pois {
        covered.insert(places::geo::encode(p.lat, p.lon, places::cache::TILE_PRECISION));
        min_lat = min_lat.min(p.lat);
        max_lat = max_lat.max(p.lat);
        min_lon = min_lon.min(p.lon);
        max_lon = max_lon.max(p.lon);
    }
    indexed += way_pois.len() as u64;

    writer.commit()?;

    // Mark the whole extract footprint as covered, not just tiles that
    // happened to contain a POI. Empty tiles inside the extract are known
    // to be empty; treating them as unfetched sends Overpass requests for
    // data already on disk.
    let mut cache = places::cache::TileCache::load(format!("{index_dir}/tiles.json"));
    if min_lat <= max_lat {
        let source = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("pbf");
        cache.mark_region(min_lat, max_lat, min_lon, max_lon, source);
        tracing::info!(
            "covered region: lat {min_lat:.2}..{max_lat:.2}, lon {min_lon:.2}..{max_lon:.2}"
        );
    }
    cache.save()?;

    gazetteer.seed_admin_areas();
    let gaz_path = format!("{index_dir}/gazetteer.json");
    gazetteer.save(&gaz_path)?;

    tracing::info!(
        "scanned {scanned} elements -> {indexed} POIs across {} tiles, {} place names",
        covered.len(),
        gazetteer.len()
    );
    Ok(())
}

fn str_tag(tags: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| tags[*k].as_str())
        .unwrap_or("")
        .to_string()
}

fn build_address(tags: &serde_json::Value) -> String {
    ["addr:housenumber", "addr:street", "addr:city"]
        .iter()
        .filter_map(|k| tags[*k].as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(serde::Deserialize)]
struct City {
    name: String,
    lat: f64,
    lon: f64,
    radius_km: f64,
}

/// Pre-seed tiles for a list of cities.
///
/// Deliberately serial with a delay between fetches. OSM grants two
/// concurrent slots and explicitly warns that heavy users may be blocked
/// without notice; a parallel blast across a hundred tiles is precisely the
/// behaviour that gets an IP banned. Slow and allowed beats fast and cut off.
fn seed_cities(
    cities_path: &str,
    index_dir: &str,
    dry_run: bool,
    delay_secs: u64,
    max_tiles: usize,
) -> Result<()> {
    let raw = std::fs::read_to_string(cities_path)
        .with_context(|| format!("reading {cities_path}"))?;
    let cities: Vec<City> = serde_json::from_str(&raw)?;

    // Work out the distinct tile set first. Cities overlap (Delhi/Noida/
    // Gurugram share tiles), so the union is smaller than the sum.
    let mut all_tiles: Vec<String> = Vec::new();
    let mut per_city = Vec::new();
    for c in &cities {
        let tiles = places::cache::tiles_for_query(c.lat, c.lon, c.radius_km * 1000.0);
        per_city.push((c.name.clone(), tiles.len()));
        for t in tiles {
            if !all_tiles.contains(&t) {
                all_tiles.push(t);
            }
        }
    }

    let cache = places::cache::TileCache::load(format!("{index_dir}/tiles.json"));
    let todo: Vec<String> = all_tiles
        .iter()
        .filter(|t| !cache.is_fresh(t, places::cache::DEFAULT_TTL_SECS))
        .cloned()
        .collect();

    let naive: usize = per_city.iter().map(|(_, n)| n).sum();
    // ~28s observed for a dense urban tile, plus the inter-request delay.
    let secs = todo.len() as u64 * (28 + delay_secs);

    println!("cities                 {}", cities.len());
    println!("tiles (sum per city)   {naive}");
    println!("tiles (distinct union) {}", all_tiles.len());
    println!("tiles already cached   {}", all_tiles.len() - todo.len());
    println!("tiles to fetch         {}", todo.len());
    println!("area covered           ~{} km2", all_tiles.len() * 25);
    println!("estimated time         ~{:.1} hours  ({} Overpass calls)",
        secs as f64 / 3600.0, todo.len());
    println!();

    let mut biggest: Vec<_> = per_city.iter().collect();
    biggest.sort_by(|a, b| b.1.cmp(&a.1));
    println!("largest: {}", biggest.iter().take(5)
        .map(|(n, t)| format!("{n} {t}"))
        .collect::<Vec<_>>().join(", "));

    if dry_run {
        println!("\n(dry run — nothing fetched)");
        return Ok(());
    }
    if todo.is_empty() {
        println!("nothing to do; all tiles are fresh");
        return Ok(());
    }

    let limit = if max_tiles == 0 { todo.len() } else { max_tiles.min(todo.len()) };
    let (index, fields) = places::schema::open_index(index_dir)?;
    let mut cache = places::cache::TileCache::load(format!("{index_dir}/tiles.json"));

    let (mut done, mut failed, mut total_places) = (0usize, 0usize, 0usize);
    for (i, tile) in todo.iter().take(limit).enumerate() {
        let Some((tlat, tlon, tradius)) = places::cache::tile_bounds(tile) else { continue };
        match places::fetch::fetch_area_blocking(tlat, tlon, tradius) {
            Ok(pois) => {
                total_places += pois.len();
                if !pois.is_empty() {
                    places::schema::add_places(&index, &fields, &pois)?;
                }
                cache.mark_fetched(tile);
                // Persist after every tile: a seed run takes hours and must
                // be resumable after a crash or a Ctrl-C.
                cache.save()?;
                done += 1;
                tracing::info!("[{}/{limit}] tile {tile}: {} places", i + 1, pois.len());
            }
            Err(e) => {
                failed += 1;
                tracing::warn!("[{}/{limit}] tile {tile} failed: {e:#}", i + 1);
            }
        }
        if i + 1 < limit {
            std::thread::sleep(std::time::Duration::from_secs(delay_secs));
        }
    }

    tracing::info!("seeded {done} tiles ({total_places} places, {failed} failed)");
    Ok(())
}

/// One way we intend to index, plus the node ids we need to locate it.
struct PendingWay {
    id: i64,
    name: String,
    brand: String,
    category: String,
    group: &'static str,
    address: String,
    phone: String,
    website: String,
    opening_hours: String,
    refs: Vec<i64>,
}

/// Pass A: find the ways worth indexing and the node ids they reference.
fn node_positions_needed(path: &str) -> Result<Vec<PendingWay>> {
    use osmpbf::{Element, ElementReader};

    let reader = ElementReader::from_path(path)?;
    let mut pending: Vec<PendingWay> = Vec::new();

    reader.for_each(|element| {
        let Element::Way(w) = element else { return };
        let tags: Vec<(String, String)> =
            w.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();

        let name = tags
            .iter()
            .find(|(k, _)| k == "name")
            .or_else(|| tags.iter().find(|(k, _)| k == "brand"))
            .map(|(_, v)| v.clone());
        let Some(name) = name else { return };

        // A `place=*` way is an administrative area, not somewhere you go.
        if tags.iter().any(|(k, _)| k == "place") {
            return;
        }

        let pairs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let Some((category, group)) = poi::categorise(pairs) else { return };

        let get = |keys: &[&str]| -> String {
            keys.iter()
                .find_map(|k| tags.iter().find(|(tk, _)| tk == k).map(|(_, v)| v.clone()))
                .unwrap_or_default()
        };

        pending.push(PendingWay {
            id: w.id(),
            name,
            brand: get(&["brand", "operator"]),
            category,
            group,
            address: [get(&["addr:housenumber"]), get(&["addr:street"]), get(&["addr:city"])]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            phone: get(&["phone", "contact:phone"]),
            website: get(&["website", "contact:website"]),
            opening_hours: get(&["opening_hours"]),
            refs: w.refs().collect(),
        });
    })?;

    tracing::info!("{} named polygon places to locate", pending.len());
    Ok(pending)
}

/// Pass B: sweep nodes for the referenced ids, then average each way's
/// vertices into a centroid.
///
/// Only the ids actually referenced are retained — holding all ~291M node
/// positions would cost several GB, while the referenced subset is a few
/// million and fits comfortably.
fn resolve_ways(path: &str, pending: &[PendingWay]) -> Result<Vec<Poi>> {
    use osmpbf::{Element, ElementReader};
    use std::collections::{HashMap, HashSet};

    let wanted: HashSet<i64> = pending.iter().flat_map(|w| w.refs.iter().copied()).collect();
    tracing::info!("resolving {} referenced node positions", wanted.len());

    let mut positions: HashMap<i64, (f64, f64)> = HashMap::with_capacity(wanted.len());
    ElementReader::from_path(path)?.for_each(|element| {
        let (id, lat, lon) = match element {
            Element::Node(n) => (n.id(), n.lat(), n.lon()),
            Element::DenseNode(n) => (n.id(), n.lat(), n.lon()),
            _ => return,
        };
        if wanted.contains(&id) {
            positions.insert(id, (lat, lon));
        }
    })?;

    let mut out = Vec::with_capacity(pending.len());
    for w in pending {
        // A closed way repeats its first node at the end; including it twice
        // would bias the centroid toward that corner.
        let mut refs: Vec<i64> = w.refs.clone();
        if refs.len() > 1 && refs.first() == refs.last() {
            refs.pop();
        }

        let coords: Vec<(f64, f64)> = refs.iter().filter_map(|id| positions.get(id).copied()).collect();
        if coords.is_empty() {
            continue; // geometry outside the extract
        }
        let lat = coords.iter().map(|c| c.0).sum::<f64>() / coords.len() as f64;
        let lon = coords.iter().map(|c| c.1).sum::<f64>() / coords.len() as f64;
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }

        out.push(Poi {
            id: format!("w{}", w.id),
            name: w.name.clone(),
            brand: w.brand.clone(),
            category: w.category.clone(),
            group: w.group.to_string(),
            lat,
            lon,
            address: w.address.clone(),
            phone: w.phone.clone(),
            website: w.website.clone(),
            opening_hours: w.opening_hours.clone(),
            prominence: 0.0,
        });
    }
    Ok(out)
}
