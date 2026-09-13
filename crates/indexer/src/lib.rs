//! Tantivy schema shared between the index builder and the query API.
//!
//! Field design is the ranking design. Splitting title / anchor / body into
//! separate fields is what makes BM25F-style weighting possible: a term
//! matching in inbound anchor text is far stronger evidence than the same
//! term buried in body copy, and you can only express that if they're
//! separate fields.

use anyhow::Result;
use tantivy::schema::{Schema, FAST, STORED, STRING, TEXT};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer};
use tantivy::{Index, IndexReader};

/// Name of the custom analyzer: lowercase + English stemming, so "booking"
/// matches "book" and "railways" matches "railway". Without stemming you
/// lose a surprising share of recall on natural queries.
pub const ANALYZER: &str = "en_stem";

#[derive(Clone)]
pub struct Fields {
    pub url: tantivy::schema::Field,
    /// Tokenized URL text — catches domain-name matches ("irctc").
    pub url_text: tantivy::schema::Field,
    pub title: tantivy::schema::Field,
    /// Inbound anchor text: what *other* pages call this one.
    pub anchor: tantivy::schema::Field,
    pub body: tantivy::schema::Field,
    pub pagerank: tantivy::schema::Field,
    pub quality: tantivy::schema::Field,
    pub inbound_domains: tantivy::schema::Field,
}

pub fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = tantivy::schema::TextOptions::default()
        .set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(ANALYZER)
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    let url = b.add_text_field("url", STRING | STORED);
    let url_text = b.add_text_field("url_text", TEXT);
    let title = b.add_text_field("title", text.clone());
    let anchor = b.add_text_field("anchor", text.clone());
    let body = b.add_text_field("body", text);
    // FAST so scoring can read them per-hit without a stored-doc fetch.
    let pagerank = b.add_f64_field("pagerank", FAST | STORED);
    let quality = b.add_f64_field("quality", FAST | STORED);
    let inbound_domains = b.add_u64_field("inbound_domains", FAST | STORED);

    let schema = b.build();
    (
        schema,
        Fields { url, url_text, title, anchor, body, pagerank, quality, inbound_domains },
    )
}

fn register_analyzer(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(Stemmer::new(tantivy::tokenizer::Language::English))
        .build();
    index.tokenizers().register(ANALYZER, analyzer);
}

pub fn open_or_create_index(path: &str) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(path)?;
    let (schema, fields) = build_schema();
    let dir = tantivy::directory::MmapDirectory::open(path)?;
    let index = Index::open_or_create(dir, schema)?;
    register_analyzer(&index);
    Ok((index, fields))
}

pub fn reader_for(index: &Index) -> Result<IndexReader> {
    Ok(index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
        .try_into()?)
}
