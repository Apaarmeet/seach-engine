//! Merge a world city list into the gazetteer.
//!
//! The places index is built from a single-country OSM extract, which is the
//! right trade for local search — but the URL resolver asks the gazetteer a
//! question local search never does: *which country is this place in?* With
//! only an Indian extract loaded, every query is implicitly Indian, so
//! "govorit moskva radio" is guessed under `.in` and `.co.in` and the one
//! suffix that could possibly work, `.ru`, is never tried. The assignment's
//! stated bar is a local radio station in Moscow, so that gap is the feature.
//!
//! GeoNames `cities15000` is 34,000 settlements over 15,000 people, with
//! coordinates, population and — the part that matters — an ISO country
//! code, plus alternate names carrying the transliterations people actually
//! type ("Moskva" for Moscow, "Bombay" for Mumbai).
//!
//! Usage:
//!   curl -O https://download.geonames.org/export/dump/cities15000.zip
//!   unzip cities15000.zip
//!   cargo run --release -p places --bin build-gazetteer -- \
//!       --geonames cities15000.txt --index-dir places-index

use anyhow::{Context, Result};
use clap::Parser;
use places::gazetteer::{Gazetteer, Place};
use std::io::BufRead;

#[derive(Parser, Debug)]
#[command(about = "Merge GeoNames world cities into an existing gazetteer")]
struct Args {
    /// Tab-separated GeoNames dump (cities15000.txt or cities5000.txt).
    #[arg(long)]
    geonames: String,
    #[arg(long, default_value = "places-index")]
    index_dir: String,
}

/// Population above which a settlement counts as a "city" for the resolver's
/// place gate. Below it, "town" — still substantial enough to disambiguate
/// an institution, which is what the gate is protecting.
const CITY_POPULATION: u64 = 100_000;

/// Population above which alternate names are also indexed.
///
/// Alternate names are where transliterations live, and they are also where
/// collisions live: GeoNames lists a Turkish town called "Of" and a
/// Tanzanian one called "Same". Restricting them to large cities keeps
/// "Moskva" and "Bombay" while keeping the long tail of common-word
/// collisions out of a table that decides whether a query word is a place.
const ALTERNATE_NAME_POPULATION: u64 = 200_000;

/// Shortest alternate name worth indexing.
const MIN_ALTERNATE_LEN: usize = 4;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let path = format!("{}/gazetteer.json", args.index_dir);
    let mut gaz = Gazetteer::load(&path);
    let before = gaz.len();
    if before == 0 {
        tracing::warn!("no existing gazetteer at {path}; creating a new one");
    }

    let file = std::fs::File::open(&args.geonames)
        .with_context(|| format!("opening {}", args.geonames))?;

    let mut cities = 0usize;
    let mut aliases = 0usize;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        let f: Vec<&str> = line.split('\t').collect();
        // geonameid, name, asciiname, alternatenames, lat, lon, class, code,
        // country, cc2, admin1..4, population, ...
        if f.len() < 15 {
            continue;
        }
        let (Ok(lat), Ok(lon)) = (f[4].parse::<f64>(), f[5].parse::<f64>()) else {
            continue;
        };
        let population: u64 = f[14].parse().unwrap_or(0);
        let country = f[8].to_ascii_lowercase();
        let kind = if population >= CITY_POPULATION { "city" } else { "town" };

        let place = |name: &str| Place {
            name: name.to_string(),
            kind: kind.to_string(),
            lat,
            lon,
            population,
            country: country.clone(),
        };

        // Non-authoritative insert: an Indian name already carried by the
        // OSM extract keeps its own entry, and lookup's population and
        // proximity ranking decides between them. Overwriting would mean a
        // world list silently replacing the higher-resolution local data.
        gaz.insert(place(f[1]));
        cities += 1;
        if f[2] != f[1] && !f[2].is_empty() {
            gaz.insert(place(f[2]));
        }

        if population >= ALTERNATE_NAME_POPULATION && !f[3].is_empty() {
            for alt in f[3].split(',') {
                let alt = alt.trim();
                if alt.len() >= MIN_ALTERNATE_LEN
                    && alt.is_ascii()
                    && alt.chars().any(|c| c.is_alphabetic())
                {
                    gaz.insert(place(alt));
                    aliases += 1;
                }
            }
        }
    }

    gaz.save(&path)?;
    tracing::info!(
        "gazetteer {path}: {before} -> {} entries (+{cities} cities, +{aliases} alternate names)",
        gaz.len()
    );
    Ok(())
}
