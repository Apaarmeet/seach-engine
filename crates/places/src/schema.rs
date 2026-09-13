//! Tantivy schema for the places index, shared by the builder and the API.

use tantivy::schema::{Schema, FAST, STORED, STRING, TEXT};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer};

/// Stemmed analyzer for place text.
///
/// Without this, plurals simply fail: measured before it was added, "cafes"
/// returned 0 results while "cafe" returned 50. The web index already had a
/// stemmer registered; the places schema was built with the default
/// tokenizer and quietly inherited none.
pub const ANALYZER: &str = "en_stem";

pub fn register_analyzer(index: &tantivy::Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(Stemmer::new(tantivy::tokenizer::Language::English))
        .build();
    index.tokenizers().register(ANALYZER, analyzer);
}

fn stemmed_text() -> tantivy::schema::TextOptions {
    tantivy::schema::TextOptions::default()
        .set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(ANALYZER)
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored()
}

pub fn build_schema() -> (Schema, PlaceFields) {
    let mut b = Schema::builder();
    let id = b.add_text_field("id", STRING | STORED);
    let name = b.add_text_field("name", stemmed_text());
    let brand = b.add_text_field("brand", stemmed_text());
    let category = b.add_text_field("category", stemmed_text());
    let group = b.add_text_field("group", stemmed_text());
    // Geohash prefixes, one term per zoom level. A radius query becomes an
    // ordinary term lookup over this field.
    // TEXT, not STRING. STRING is un-tokenized, so the space-joined prefix
    // list would be stored as a single opaque term and a lookup for any one
    // cell would never match. TEXT tokenizes on whitespace, giving one term
    // per zoom level, which is the whole point of indexing prefixes.
    let geocell = b.add_text_field("geocell", TEXT);
    let lat = b.add_f64_field("lat", FAST | STORED);
    let lon = b.add_f64_field("lon", FAST | STORED);
    let prominence = b.add_f64_field("prominence", FAST | STORED);
    let address = b.add_text_field("address", TEXT | STORED);
    let phone = b.add_text_field("phone", STORED);
    let website = b.add_text_field("website", STORED);
    let opening_hours = b.add_text_field("opening_hours", STORED);
    let schema = b.build();
    (
        schema,
        PlaceFields {
            id, name, brand, category, group, geocell, lat, lon, prominence,
            address, phone, website, opening_hours,
        },
    )
}

#[derive(Clone)]
pub struct PlaceFields {
    pub id: tantivy::schema::Field,
    pub name: tantivy::schema::Field,
    pub brand: tantivy::schema::Field,
    pub category: tantivy::schema::Field,
    pub group: tantivy::schema::Field,
    pub geocell: tantivy::schema::Field,
    pub lat: tantivy::schema::Field,
    pub lon: tantivy::schema::Field,
    pub prominence: tantivy::schema::Field,
    pub address: tantivy::schema::Field,
    pub phone: tantivy::schema::Field,
    pub website: tantivy::schema::Field,
    pub opening_hours: tantivy::schema::Field,
}


/// Shortest geohash prefix indexed per place. Nothing coarser is useful for
/// "near me" — a 2-character cell spans most of a subcontinent.
pub const MIN_PREFIX: usize = 3;
pub const MAX_PREFIX: usize = 9;

/// Add places to an index. Shared by the CLI builder and the API's lazy
/// fetch path so both produce byte-identical documents.
pub fn open_index(dir: &str) -> anyhow::Result<(tantivy::Index, PlaceFields)> {
    std::fs::create_dir_all(dir)?;
    let (schema, fields) = build_schema();
    let mmap = tantivy::directory::MmapDirectory::open(dir)?;
    let index = tantivy::Index::open_or_create(mmap, schema)?;
    register_analyzer(&index);
    Ok((index, fields))
}

pub fn add_places(
    index: &tantivy::Index,
    f: &PlaceFields,
    pois: &[crate::poi::Poi],
) -> anyhow::Result<()> {
    use tantivy::doc;
    let mut writer = index.writer(50_000_000)?;
    for p in pois {
        // Idempotent by OSM id. Without this, any place covered by two
        // fetches — overlapping tiles, a re-fetch after TTL, or a PBF
        // ingest layered over lazily-fetched tiles — appears twice in
        // results. Tantivy orders by opstamp, so deleting before adding in
        // the same commit leaves exactly the new document.
        writer.delete_term(tantivy::Term::from_field_text(f.id, &p.id));
        let hash = crate::geo::encode(p.lat, p.lon, MAX_PREFIX);
        let cells = crate::geo::prefixes(&hash, MIN_PREFIX).join(" ");
        writer.add_document(doc!(
            f.id            => p.id.clone(),
            f.name          => p.name.clone(),
            f.brand         => p.brand.clone(),
            f.category      => p.category.clone(),
            f.group         => p.group.clone(),
            f.geocell       => cells,
            f.lat           => p.lat,
            f.lon           => p.lon,
            f.prominence    => crate::poi::prominence(p) as f64,
            f.address       => p.address.clone(),
            f.phone         => p.phone.clone(),
            f.website       => p.website.clone(),
            f.opening_hours => p.opening_hours.clone(),
        ))?;
    }
    writer.commit()?;
    Ok(())
}

/// Add a batch of places using an existing writer.
///
/// Separate from `add_places` because bulk ingest must not create and commit
/// a writer per batch — that would force a segment flush every 50k docs and
/// turn a minutes-long job into an hours-long one.
///
/// `dedupe` controls whether each document is preceded by a delete on its id.
/// Incremental writes need it (overlapping tiles re-add the same places), but
/// a bulk load into a freshly-cleared index does not: OSM ids are unique
/// within an extract, so every delete is a no-op, and millions of no-op
/// deletes are real work at commit time for no benefit.
pub fn write_batch(
    writer: &mut tantivy::IndexWriter,
    f: &PlaceFields,
    pois: &[crate::poi::Poi],
    dedupe: bool,
) -> anyhow::Result<()> {
    use tantivy::doc;
    for p in pois {
        if dedupe {
            writer.delete_term(tantivy::Term::from_field_text(f.id, &p.id));
        }
        let hash = crate::geo::encode(p.lat, p.lon, MAX_PREFIX);
        let cells = crate::geo::prefixes(&hash, MIN_PREFIX).join(" ");
        writer.add_document(doc!(
            f.id            => p.id.clone(),
            f.name          => p.name.clone(),
            f.brand         => p.brand.clone(),
            f.category      => p.category.clone(),
            f.group         => p.group.clone(),
            f.geocell       => cells,
            f.lat           => p.lat,
            f.lon           => p.lon,
            f.prominence    => crate::poi::prominence(p) as f64,
            f.address       => p.address.clone(),
            f.phone         => p.phone.clone(),
            f.website       => p.website.clone(),
            f.opening_hours => p.opening_hours.clone(),
        ))?;
    }
    Ok(())
}
