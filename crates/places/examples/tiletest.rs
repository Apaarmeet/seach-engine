fn main() {
    for (name, lat, lon, r) in [("Mumbai",19.0760,72.8777,22000.0),("Koramangala",12.9352,77.6245,800.0)] {
        let tiles = places::cache::tiles_for_query(lat, lon, r);
        println!("{name}: {} tiles, lengths={:?}", tiles.len(),
            tiles.iter().map(|t| t.len()).collect::<std::collections::BTreeSet<_>>());
        if let Some(t) = tiles.first() {
            let (tlat,tlon,trad) = places::cache::tile_bounds(t).unwrap();
            println!("   first tile {t} -> overpass radius {:.0} m  (lat {tlat:.3}, lon {tlon:.3})", trad);
        }
    }
}
