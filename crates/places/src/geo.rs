//! Geospatial primitives: distance, geohashing, and radius cell covering.
//!
//! The central problem of local search is that you cannot compute distance
//! to five million places on every query. You need to cheaply reduce
//! candidates to "things plausibly nearby", then compute exact distance on
//! the survivors. Geohashing is how that reduction happens here.
//!
//! A geohash interleaves latitude and longitude bits into a base-32 string,
//! so a *shared prefix means spatial proximity*: everything in `tdr1y` sits
//! inside one box. That turns "find things near me" into an ordinary
//! inverted-index term lookup, which the existing tantivy machinery already
//! does well — no separate spatial database required.

const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Great-circle distance in metres.
///
/// Haversine rather than a flat-earth approximation: at India's latitudes a
/// naive euclidean-on-degrees calculation is wrong by enough to reorder
/// nearby results, and it fails outright across the antimeridian.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

/// Encode a coordinate as a geohash of the given precision (1..=12).
pub fn encode(lat: f64, lon: f64, precision: usize) -> String {
    let precision = precision.clamp(1, 12);
    let (mut lat_range, mut lon_range) = ((-90.0f64, 90.0f64), (-180.0f64, 180.0f64));
    let mut out = String::with_capacity(precision);
    let (mut bit, mut ch, mut even) = (0u8, 0usize, true);

    while out.len() < precision {
        if even {
            let mid = (lon_range.0 + lon_range.1) / 2.0;
            if lon > mid {
                ch = (ch << 1) | 1;
                lon_range.0 = mid;
            } else {
                ch <<= 1;
                lon_range.1 = mid;
            }
        } else {
            let mid = (lat_range.0 + lat_range.1) / 2.0;
            if lat > mid {
                ch = (ch << 1) | 1;
                lat_range.0 = mid;
            } else {
                ch <<= 1;
                lat_range.1 = mid;
            }
        }
        even = !even;
        bit += 1;
        if bit == 5 {
            out.push(BASE32[ch] as char);
            bit = 0;
            ch = 0;
        }
    }
    out
}

/// Decode a geohash to the centre and half-extents of its cell.
pub fn decode(hash: &str) -> Option<(f64, f64, f64, f64)> {
    let (mut lat_range, mut lon_range) = ((-90.0f64, 90.0f64), (-180.0f64, 180.0f64));
    let mut even = true;

    for c in hash.chars() {
        let idx = BASE32.iter().position(|&b| b as char == c)?;
        for i in (0..5).rev() {
            let bit = (idx >> i) & 1;
            if even {
                let mid = (lon_range.0 + lon_range.1) / 2.0;
                if bit == 1 {
                    lon_range.0 = mid;
                } else {
                    lon_range.1 = mid;
                }
            } else {
                let mid = (lat_range.0 + lat_range.1) / 2.0;
                if bit == 1 {
                    lat_range.0 = mid;
                } else {
                    lat_range.1 = mid;
                }
            }
            even = !even;
        }
    }

    Some((
        (lat_range.0 + lat_range.1) / 2.0,
        (lon_range.0 + lon_range.1) / 2.0,
        (lat_range.1 - lat_range.0) / 2.0,
        (lon_range.1 - lon_range.0) / 2.0,
    ))
}

/// Approximate cell dimensions (height_m, width_m) at a given precision and
/// latitude. Width shrinks with latitude because meridians converge.
pub fn cell_size_m(precision: usize, lat: f64) -> (f64, f64) {
    // Each character adds 5 bits, alternating lon/lat.
    let bits = precision * 5;
    let lat_bits = bits / 2;
    let lon_bits = bits - lat_bits;
    let lat_span = 180.0 / (1u64 << lat_bits) as f64;
    let lon_span = 360.0 / (1u64 << lon_bits) as f64;
    let h = lat_span / 360.0 * (2.0 * std::f64::consts::PI * EARTH_RADIUS_M);
    let w = lon_span / 360.0 * (2.0 * std::f64::consts::PI * EARTH_RADIUS_M) * lat.to_radians().cos();
    (h, w)
}

/// Pick the smallest precision whose cells are still at least as large as
/// `radius_m`, so a circle of that radius is covered by a handful of cells
/// rather than thousands.
pub fn precision_for_radius(radius_m: f64, lat: f64) -> usize {
    for p in (1..=9).rev() {
        let (h, w) = cell_size_m(p, lat);
        if h.min(w) >= radius_m {
            return p;
        }
    }
    1
}

/// The set of geohash cells covering a circle.
///
/// Returns the centre cell plus its eight neighbours. This matters more than
/// it looks: without the neighbours, a place 50m away but on the other side
/// of a cell boundary is invisible to the query. That class of bug produces
/// a search that works "except sometimes", which is the worst kind.
pub fn cells_covering(lat: f64, lon: f64, radius_m: f64) -> Vec<String> {
    let precision = precision_for_radius(radius_m, lat);
    let (h, w) = cell_size_m(precision, lat);

    // How many cells out we must step to span the radius in each direction.
    let steps_lat = (radius_m / h).ceil().max(1.0) as i32;
    let steps_lon = (radius_m / w.max(1.0)).ceil().max(1.0) as i32;

    // Degree deltas for one cell step.
    let d_lat = h / EARTH_RADIUS_M * (180.0 / std::f64::consts::PI);
    let cos_lat = lat.to_radians().cos().max(1e-6);
    let d_lon = w / (EARTH_RADIUS_M * cos_lat) * (180.0 / std::f64::consts::PI);

    let mut out = Vec::new();
    for i in -steps_lat..=steps_lat {
        for j in -steps_lon..=steps_lon {
            let plat = (lat + i as f64 * d_lat).clamp(-90.0, 90.0);
            let mut plon = lon + j as f64 * d_lon;
            // Wrap across the antimeridian rather than clamping, or queries
            // near ±180° silently lose half their neighbours.
            if plon > 180.0 {
                plon -= 360.0;
            } else if plon < -180.0 {
                plon += 360.0;
            }
            let cell = encode(plat, plon, precision);
            if !out.contains(&cell) {
                out.push(cell);
            }
        }
    }
    out
}

/// All prefixes of a geohash, used at index time so a place is findable at
/// every zoom level with a plain term lookup.
pub fn prefixes(hash: &str, min_len: usize) -> Vec<String> {
    (min_len..=hash.len()).map(|n| hash[..n].to_string()).collect()
}

/// Distance decay, mapping metres to a 0..1 desirability score.
///
/// Deliberately non-linear. The difference between 100 m and 600 m is the
/// difference between "walk there" and "maybe not"; the difference between
/// 9 km and 9.5 km is nothing. A linear penalty gets this backwards and
/// makes far-away results feel almost as good as near ones.
pub fn distance_decay(distance_m: f64, scale_m: f64) -> f64 {
    if scale_m <= 0.0 {
        return 0.0;
    }
    (-distance_m / scale_m).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bengaluru city centre.
    const BLR: (f64, f64) = (12.9716, 77.5946);

    #[test]
    fn haversine_matches_known_distances() {
        // Bengaluru -> Chennai is ~290 km.
        let d = haversine_m(BLR.0, BLR.1, 13.0827, 80.2707) / 1000.0;
        assert!((d - 290.0).abs() < 15.0, "got {d} km, expected ~290");

        // Identical points are zero.
        assert!(haversine_m(BLR.0, BLR.1, BLR.0, BLR.1) < 1e-6);
    }

    #[test]
    fn haversine_is_symmetric() {
        let a = haversine_m(BLR.0, BLR.1, 19.0760, 72.8777);
        let b = haversine_m(19.0760, 72.8777, BLR.0, BLR.1);
        assert!((a - b).abs() < 1e-6);
    }

    #[test]
    fn geohash_roundtrips_within_cell() {
        let hash = encode(BLR.0, BLR.1, 9);
        let (lat, lon, dlat, dlon) = decode(&hash).unwrap();
        assert!((lat - BLR.0).abs() <= dlat);
        assert!((lon - BLR.1).abs() <= dlon);
    }

    #[test]
    fn nearby_points_share_a_prefix() {
        // Two points ~200 m apart should agree to at least 6 characters.
        let a = encode(12.9716, 77.5946, 9);
        let b = encode(12.9734, 77.5946, 9);
        let shared = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
        assert!(shared >= 6, "only {shared} shared chars between {a} and {b}");
    }

    #[test]
    fn distant_points_do_not_share_a_prefix() {
        let blr = encode(BLR.0, BLR.1, 9);
        let del = encode(28.6139, 77.2090, 9);
        assert_ne!(blr[..3].to_string(), del[..3].to_string());
    }

    #[test]
    fn covering_includes_the_centre_cell() {
        let cells = cells_covering(BLR.0, BLR.1, 1000.0);
        let precision = precision_for_radius(1000.0, BLR.0);
        assert!(cells.contains(&encode(BLR.0, BLR.1, precision)));
    }

    /// The boundary bug this function exists to prevent: a point just across
    /// a cell edge must still be covered.
    #[test]
    fn covering_reaches_across_cell_boundaries() {
        let radius = 800.0;
        let cells = cells_covering(BLR.0, BLR.1, radius);
        let precision = precision_for_radius(radius, BLR.0);

        // Walk outward in several directions; anything inside the radius
        // must live in one of the covering cells.
        for (dlat, dlon) in [(0.005, 0.0), (-0.005, 0.0), (0.0, 0.005), (0.0, -0.005)] {
            let (plat, plon) = (BLR.0 + dlat, BLR.1 + dlon);
            if haversine_m(BLR.0, BLR.1, plat, plon) <= radius {
                let cell = encode(plat, plon, precision);
                assert!(
                    cells.contains(&cell),
                    "point ({plat},{plon}) in cell {cell} not covered by {cells:?}"
                );
            }
        }
    }

    #[test]
    fn larger_radius_uses_coarser_precision() {
        let near = precision_for_radius(200.0, BLR.0);
        let far = precision_for_radius(20_000.0, BLR.0);
        assert!(far < near, "20km precision {far} should be coarser than 200m {near}");
    }

    #[test]
    fn prefixes_are_nested() {
        let p = prefixes("tdr1y8k", 3);
        assert_eq!(p.first().unwrap(), "tdr");
        assert_eq!(p.last().unwrap(), "tdr1y8k");
        assert!(p.windows(2).all(|w| w[1].starts_with(&w[0])));
    }

    #[test]
    fn decay_is_monotonic_and_bounded() {
        assert!((distance_decay(0.0, 1000.0) - 1.0).abs() < 1e-9);
        assert!(distance_decay(500.0, 1000.0) > distance_decay(2000.0, 1000.0));
        assert!(distance_decay(1e9, 1000.0) >= 0.0);
    }

    /// Encodes the intent behind using exponential rather than linear decay.
    #[test]
    fn decay_punishes_near_distances_more_than_far_ones() {
        let scale = 1000.0;
        let near_drop = distance_decay(100.0, scale) - distance_decay(600.0, scale);
        let far_drop = distance_decay(9000.0, scale) - distance_decay(9500.0, scale);
        assert!(
            near_drop > far_drop * 10.0,
            "500m near the user ({near_drop}) should matter far more than 500m far away ({far_drop})"
        );
    }
}
