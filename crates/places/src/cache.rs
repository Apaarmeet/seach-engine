//! Lazy tile cache: fetch places on demand, keep them, serve locally.
//!
//! This exists because both obvious designs are wrong.
//!
//! *Query Overpass per request* — measured at 1.2–7.7 s with a failure in
//! 1 of 3 samples, against 0 ms locally. OSM's usage policy also asks heavy
//! users to run their own server and warns that clients "may be blocked
//! without notice". It is a community resource, not a backend.
//!
//! *Download the whole 1.71 GB country extract up front* — pays for all of
//! India to answer a query about one neighbourhood, and is stale the moment
//! it lands.
//!
//! So: divide the world into geohash tiles, fetch a tile the first time
//! someone searches inside it, index it, and serve every later query in
//! that area locally. Cost is proportional to where your users actually
//! are, data refreshes per-tile on a TTL, and the API is hit rarely enough
//! to stay comfortably inside the rate limits.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Geohash precision for a cache tile: 5 chars ≈ 4.9 km × 4.9 km.
///
/// The tradeoff is real. Finer tiles mean more Overpass round-trips for a
/// moving user; coarser tiles mean each miss downloads a large area and
/// takes longer. 5 keeps a typical 1–2 km query inside one to four tiles.
pub const TILE_PRECISION: usize = 5;

/// How long a tile stays fresh. OSM edits continuously but POI churn is
/// slow — a week is generous for names and locations, and far too long if
/// you ever add opening hours, which change constantly.
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 3600;

/// A rectangle whose contents are already held locally.
///
/// Bulk ingest needs this. Marking only the tiles that happened to contain a
/// POI leaves every empty tile — water, farmland, a gap in the data — looking
/// unfetched, so a wide query fires Overpass calls for ground already
/// covered. Measured: one "hospital in Chennai" produced 38 cache misses
/// against an index that already held all of Chennai, and hung for minutes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoveredRegion {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
    pub fetched_at: u64,
    #[serde(default)]
    pub source: String,
}

impl CoveredRegion {
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TileCache {
    /// tile geohash -> unix seconds when it was fetched
    tiles: HashMap<String, u64>,
    /// Areas covered wholesale by a bulk ingest.
    #[serde(default)]
    regions: Vec<CoveredRegion>,
    #[serde(skip)]
    path: PathBuf,
}

impl TileCache {
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut cache: TileCache = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cache.path = path;
        cache
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn is_fresh(&self, tile: &str, ttl_secs: u64) -> bool {
        if let Some(&fetched) = self.tiles.get(tile) {
            if now().saturating_sub(fetched) < ttl_secs {
                return true;
            }
        }
        // A tile inside a bulk-covered region needs no fetch even if it
        // holds nothing — the absence of POIs there is itself known data.
        let Some((lat, lon, _, _)) = crate::geo::decode(tile) else {
            return false;
        };
        self.regions
            .iter()
            .any(|r| r.contains(lat, lon) && now().saturating_sub(r.fetched_at) < ttl_secs)
    }

    /// Record that everything inside a rectangle is held locally.
    pub fn mark_region(&mut self, min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64, source: &str) {
        self.regions.retain(|r| r.source != source);
        self.regions.push(CoveredRegion {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
            fetched_at: now(),
            source: source.to_string(),
        });
    }

    pub fn regions(&self) -> &[CoveredRegion] {
        &self.regions
    }

    pub fn mark_fetched(&mut self, tile: &str) {
        self.tiles.insert(tile.to_string(), now());
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Which of these tiles need fetching.
    pub fn stale_tiles(&self, tiles: &[String], ttl_secs: u64) -> Vec<String> {
        tiles
            .iter()
            .filter(|t| !self.is_fresh(t, ttl_secs))
            .cloned()
            .collect()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Does a tile's box actually overlap the query circle?
///
/// Cheap rectangle-circle test: clamp the circle's centre into the tile's
/// extent to find the nearest point on the tile, then measure to it.
fn tile_intersects_circle(tile: &str, lat: f64, lon: f64, radius_m: f64) -> bool {
    let Some((clat, clon, dlat, dlon)) = crate::geo::decode(tile) else {
        return false;
    };
    let nearest_lat = lat.clamp(clat - dlat, clat + dlat);
    let nearest_lon = lon.clamp(clon - dlon, clon + dlon);
    crate::geo::haversine_m(lat, lon, nearest_lat, nearest_lon) <= radius_m
}

/// Tiles that must be *fetched* to answer a query.
///
/// Distinct from `geo::cells_covering`, which is the *query* path. That one
/// returns a 3×3 neighbourhood because an extra term lookup costs nothing
/// and missing a boundary case costs correctness. Fetching is the opposite:
/// each tile is a ~5 km Overpass download, so pulling the full neighbourhood
/// for a 1 km query downloaded ~175 km² of data and took 135 seconds.
///
/// So here we keep only tiles the circle genuinely overlaps. A query
/// entirely inside one tile fetches exactly one tile.
///
/// Note the whole tile is still fetched, not just the query radius — the
/// next query from 300 m down the road should be a cache hit, not another
/// round-trip.
pub fn tiles_for_query(lat: f64, lon: f64, radius_m: f64) -> Vec<String> {
    // Enumerate the grid at TILE_PRECISION directly rather than deriving it
    // from `geo::cells_covering`.
    //
    // Deriving was a bug: cells_covering chooses its precision from the
    // radius, so a 22 km query produced precision-3 cells (~156 km across).
    // Those are coarser than TILE_PRECISION, so truncation didn't fire and
    // they passed through whole — Mumbai came back as 2 tiles instead of
    // ~60, and one of them would have asked Overpass for a 100 km radius.
    let mut tiles: Vec<String> = Vec::new();

    let (cell_h, cell_w) = crate::geo::cell_size_m(TILE_PRECISION, lat);
    if cell_h <= 0.0 || cell_w <= 0.0 {
        return vec![crate::geo::encode(lat, lon, TILE_PRECISION)];
    }

    // Step at half a cell so no cell in the box can be stepped over.
    let deg_per_m_lat = 1.0 / 111_320.0;
    let cos_lat = lat.to_radians().cos().abs().max(1e-6);
    let deg_per_m_lon = 1.0 / (111_320.0 * cos_lat);

    let step_lat = cell_h * 0.5 * deg_per_m_lat;
    let step_lon = cell_w * 0.5 * deg_per_m_lon;
    let span_lat = radius_m * deg_per_m_lat;
    let span_lon = radius_m * deg_per_m_lon;

    let steps_lat = (span_lat / step_lat).ceil() as i64;
    let steps_lon = (span_lon / step_lon).ceil() as i64;

    for i in -steps_lat..=steps_lat {
        for j in -steps_lon..=steps_lon {
            let plat = (lat + i as f64 * step_lat).clamp(-90.0, 90.0);
            let mut plon = lon + j as f64 * step_lon;
            if plon > 180.0 {
                plon -= 360.0;
            } else if plon < -180.0 {
                plon += 360.0;
            }
            let tile = crate::geo::encode(plat, plon, TILE_PRECISION);
            if !tiles.contains(&tile) && tile_intersects_circle(&tile, lat, lon, radius_m) {
                tiles.push(tile);
            }
        }
    }

    // A radius smaller than one cell can still miss if the centre sits near
    // an edge and every sample lands in a neighbour.
    let centre = crate::geo::encode(lat, lon, TILE_PRECISION);
    if !tiles.contains(&centre) {
        tiles.push(centre);
    }
    tiles
}

/// Centre and radius that cover an entire tile, for the Overpass request.
pub fn tile_bounds(tile: &str) -> Option<(f64, f64, f64)> {
    let (lat, lon, dlat, dlon) = crate::geo::decode(tile)?;
    // Radius of the circle circumscribing the tile, so nothing in a corner
    // is missed. A little overfetch here is much cheaper than a gap.
    let corner = crate::geo::haversine_m(lat, lon, lat + dlat, lon + dlon);
    Some((lat, lon, corner * 1.05))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_after_marking_stale_after_ttl() {
        let mut c = TileCache::default();
        assert!(!c.is_fresh("tdr1y", 3600));
        c.mark_fetched("tdr1y");
        assert!(c.is_fresh("tdr1y", 3600));
        // Zero TTL means nothing is ever fresh.
        assert!(!c.is_fresh("tdr1y", 0));
    }

    #[test]
    fn stale_tiles_lists_only_what_is_missing() {
        let mut c = TileCache::default();
        c.mark_fetched("aaaaa");
        let want = vec!["aaaaa".to_string(), "bbbbb".to_string()];
        assert_eq!(c.stale_tiles(&want, 3600), vec!["bbbbb".to_string()]);
    }

    #[test]
    fn tiles_are_at_tile_precision() {
        let tiles = tiles_for_query(12.9716, 77.5946, 1000.0);
        assert!(!tiles.is_empty());
        assert!(tiles.iter().all(|t| t.len() == TILE_PRECISION), "got {tiles:?}");
    }

    #[test]
    fn small_radius_still_yields_the_containing_tile() {
        let tiles = tiles_for_query(12.9716, 77.5946, 50.0);
        let centre = crate::geo::encode(12.9716, 77.5946, TILE_PRECISION);
        assert!(tiles.contains(&centre));
    }

    #[test]
    fn large_radius_spans_multiple_tiles() {
        let small = tiles_for_query(12.9716, 77.5946, 500.0);
        let large = tiles_for_query(12.9716, 77.5946, 20_000.0);
        assert!(large.len() > small.len(), "small {} large {}", small.len(), large.len());
    }

    /// Regression test for a measured bug: fetching the full 3x3 cell
    /// neighbourhood pulled 7 tiles (~175 km2) for a 1 km query and took
    /// 135 seconds. A small query must fetch exactly the tile it sits in.
    #[test]
    fn small_query_fetches_only_the_containing_tile() {
        // Deep inside a tile, far from any edge.
        let tile = crate::geo::encode(12.9352, 77.6245, TILE_PRECISION);
        let (clat, clon, _, _) = crate::geo::decode(&tile).unwrap();
        let tiles = tiles_for_query(clat, clon, 500.0);
        assert_eq!(tiles.len(), 1, "expected 1 tile, got {tiles:?}");
    }

    #[test]
    fn query_near_an_edge_includes_the_neighbour() {
        let tile = crate::geo::encode(12.9352, 77.6245, TILE_PRECISION);
        let (clat, clon, dlat, _) = crate::geo::decode(&tile).unwrap();
        // Sit just inside the northern edge with a radius that crosses it.
        let near_edge = clat + dlat * 0.98;
        let tiles = tiles_for_query(near_edge, clon, 1500.0);
        assert!(tiles.len() >= 2, "expected a neighbour, got {tiles:?}");
    }

    /// Regression test: a 22 km query over Mumbai returned 2 tiles, one of
    /// them precision-3 (~156 km across), because coarse cells from
    /// `cells_covering` passed through untruncated. Every tile must be at
    /// TILE_PRECISION, and a big city must need a lot of them.
    #[test]
    fn large_city_radius_yields_many_uniform_tiles() {
        let tiles = tiles_for_query(19.0760, 72.8777, 22_000.0);
        assert!(
            tiles.iter().all(|t| t.len() == TILE_PRECISION),
            "mixed precisions: {:?}",
            tiles.iter().map(|t| t.len()).collect::<std::collections::BTreeSet<_>>()
        );
        // pi*22^2 ~= 1520 km2 at ~25 km2 per tile ~= 60 tiles.
        assert!(tiles.len() >= 40, "expected ~60 tiles for a 22km radius, got {}", tiles.len());
    }

    /// Every point inside the circle must fall in some returned tile —
    /// the property that actually matters for coverage.
    #[test]
    fn covering_leaves_no_gaps_inside_the_radius() {
        let (lat, lon, radius) = (19.0760, 72.8777, 12_000.0);
        let tiles = tiles_for_query(lat, lon, radius);
        let deg_lat = 1.0 / 111_320.0;
        let deg_lon = 1.0 / (111_320.0 * lat.to_radians().cos());
        // Sample a grid across the circle.
        for i in -10..=10 {
            for j in -10..=10 {
                let plat = lat + (i as f64 / 10.0) * radius * deg_lat;
                let plon = lon + (j as f64 / 10.0) * radius * deg_lon;
                if crate::geo::haversine_m(lat, lon, plat, plon) > radius {
                    continue;
                }
                let t = crate::geo::encode(plat, plon, TILE_PRECISION);
                assert!(tiles.contains(&t), "gap: ({plat},{plon}) in tile {t} not covered");
            }
        }
    }

    #[test]
    fn every_returned_tile_actually_intersects() {
        let tiles = tiles_for_query(12.9716, 77.5946, 3000.0);
        for t in &tiles {
            assert!(
                tile_intersects_circle(t, 12.9716, 77.5946, 3000.0),
                "tile {t} does not intersect the query circle"
            );
        }
    }

    #[test]
    fn tile_bounds_cover_the_whole_tile() {
        let tile = crate::geo::encode(12.9716, 77.5946, TILE_PRECISION);
        let (lat, lon, radius) = tile_bounds(&tile).unwrap();
        let (_, _, dlat, dlon) = crate::geo::decode(&tile).unwrap();
        // Every corner must fall inside the fetch radius.
        for (sy, sx) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
            let d = crate::geo::haversine_m(lat, lon, lat + sy * dlat, lon + sx * dlon);
            assert!(d <= radius, "corner at {d}m outside fetch radius {radius}m");
        }
    }

    /// Regression test for the Chennai hang: a bulk-covered area must not
    /// produce cache misses, including for tiles that hold no POIs.
    #[test]
    fn bulk_region_covers_tiles_with_no_pois() {
        let mut c = TileCache::default();
        // Roughly India.
        c.mark_region(6.0, 36.0, 68.0, 98.0, "india.osm.pbf");

        let chennai = crate::geo::encode(13.0827, 80.2707, TILE_PRECISION);
        let empty_bay = crate::geo::encode(13.5, 81.5, TILE_PRECISION);
        assert!(c.is_fresh(&chennai, 3600), "Chennai should be covered");
        assert!(c.is_fresh(&empty_bay, 3600), "empty tile inside region is still covered");

        // Outside the region, lazy fetch should still apply.
        let london = crate::geo::encode(51.5, -0.12, TILE_PRECISION);
        assert!(!c.is_fresh(&london, 3600));
    }

    #[test]
    fn a_wide_query_inside_a_bulk_region_needs_no_fetches() {
        let mut c = TileCache::default();
        c.mark_region(6.0, 36.0, 68.0, 98.0, "india.osm.pbf");
        // 12 km city radius, the case that hung.
        let tiles = tiles_for_query(13.0827, 80.2707, 12_000.0);
        assert!(tiles.len() > 10, "expected many tiles, got {}", tiles.len());
        assert!(c.stale_tiles(&tiles, 3600).is_empty(), "no tile should need fetching");
    }

    #[test]
    fn expired_region_stops_covering() {
        let mut c = TileCache::default();
        c.mark_region(6.0, 36.0, 68.0, 98.0, "x");
        let t = crate::geo::encode(13.0, 80.0, TILE_PRECISION);
        assert!(c.is_fresh(&t, 3600));
        assert!(!c.is_fresh(&t, 0), "zero TTL expires regions too");
    }

    #[test]
    fn cache_survives_a_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tilecache-{}", now()));
        let path = dir.join("tiles.json");
        let mut c = TileCache::load(&path);
        c.mark_fetched("tdr1y");
        c.save().unwrap();

        let reloaded = TileCache::load(&path);
        assert!(reloaded.is_fresh("tdr1y", 3600));
        assert_eq!(reloaded.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
