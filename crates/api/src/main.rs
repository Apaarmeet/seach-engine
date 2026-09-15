mod direct;
mod nearby;
mod rank;

use anyhow::Result;
use axum::extract::{Query as AxumQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use common::{SearchResponse, SearchResult};
use indexer::Fields;
use rank::Weights;
use serde::Deserialize;
use std::sync::Arc;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser};
use tantivy::schema::Value;
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, TantivyDocument};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

struct Shard {
    index: Index,
    fields: Fields,
    reader: IndexReader,
    /// Corpus-relative scale for authority saturation, measured at startup.
    median_pagerank: f32,
}

struct AppState {
    shards: Vec<Shard>,
    weights: Weights,
}

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "index", value_delimiter = ',')]
    index_dirs: Vec<String>,
    /// Port to listen on.
    ///
    /// Reads `PORT` from the environment because most container platforms
    /// (Railway, Render, Cloud Run, Heroku) assign one dynamically and route
    /// only to that port. Hardcoding 8080 makes the deploy look healthy while
    /// the public URL returns 502 — the platform is forwarding somewhere the
    /// process never bound.
    #[arg(long, env = "PORT", default_value_t = 8080)]
    port: u16,
    /// Places index for local ("near me") search.
    #[arg(long, default_value = "places-index")]
    places_dir: String,
    /// Disable on-demand Overpass fetching; serve only pre-indexed places.
    #[arg(long)]
    no_lazy_fetch: bool,
    /// Seconds before a cached tile is refetched.
    #[arg(long, default_value_t = 604800)]
    tile_ttl: u64,
    /// Directory of built frontend assets to serve. In development Vite
    /// serves these and proxies the API; in production one binary serves
    /// both, so there is nothing to coordinate and no CORS surface.
    #[arg(long)]
    static_dir: Option<String>,

    // Ranking weights, exposed so ablations can be run without recompiling.
    // This is what makes "how much is anchor text actually worth?" an
    // experiment with a number attached rather than an opinion.
    #[arg(long)]
    anchor_boost: Option<f32>,
    #[arg(long)]
    title_boost: Option<f32>,
    #[arg(long)]
    pagerank_weight: Option<f32>,
    #[arg(long)]
    text_weight: Option<f32>,
    #[arg(long)]
    quality_weight: Option<f32>,
    #[arg(long)]
    domain_trust_weight: Option<f32>,
}

impl Args {
    fn weights(&self) -> Weights {
        let d = Weights::default();
        Weights {
            anchor_boost: self.anchor_boost.unwrap_or(d.anchor_boost),
            title_boost: self.title_boost.unwrap_or(d.title_boost),
            pagerank_weight: self.pagerank_weight.unwrap_or(d.pagerank_weight),
            text_weight: self.text_weight.unwrap_or(d.text_weight),
            quality_weight: self.quality_weight.unwrap_or(d.quality_weight),
            domain_trust_weight: self.domain_trust_weight.unwrap_or(d.domain_trust_weight),
            ..d
        }
    }
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Return the per-signal score breakdown.
    #[serde(default)]
    explain: bool,
}
fn default_limit() -> usize {
    10
}

/// API error carrying the status it should surface as.
///
/// Bad client input must be 4xx. Reporting "lat=91" as a 500 tells the
/// caller the server broke when in fact the request was invalid, and it
/// pollutes error-rate monitoring with other people's typos.
pub struct AppError {
    pub status: StatusCode,
    pub err: anyhow::Error,
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        AppError { status: StatusCode::BAD_REQUEST, err: anyhow::anyhow!(msg.into()) }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!("{:#}", self.err);
        } else {
            tracing::debug!("client error: {:#}", self.err);
        }
        let body = serde_json::json!({ "error": self.err.to_string() });
        (self.status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError { status: StatusCode::INTERNAL_SERVER_ERROR, err: e }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mut shards = Vec::new();
    for dir in &args.index_dirs {
        let (index, fields) = indexer::open_or_create_index(dir)?;
        let reader = indexer::reader_for(&index)?;
        let median_pagerank = measure_median_pagerank(&reader, &fields);
        tracing::info!(
            "shard {dir}: {} docs, median pagerank {median_pagerank:.3e}",
            reader.searcher().num_docs()
        );
        shards.push(Shard { index, fields, reader, median_pagerank });
    }

    let weights = args.weights();
    tracing::info!("ranking weights: {weights:?}");
    let state = Arc::new(AppState { shards, weights });

    // Local search lives on its own router with its own state: the places
    // index answers a different question from the web index and shares no
    // ranking code with it.
    let places = Arc::new(nearby::open_places(
        &args.places_dir,
        !args.no_lazy_fetch,
        args.tile_ttl,
    )?);
    // The resolver shares the places gazetteer: the same 300k place names
    // that answer "in Shillong" also tell it that "shillong" is a place
    // worth keeping in a hostname guess, and which ccTLD to try.
    let direct = Arc::new(direct::open(places.clone())?);
    tracing::info!(
        "places index {}: {} places",
        args.places_dir,
        places.reader.searcher().num_docs()
    );

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/search", get(search_handler))
        .route("/stats", get(stats_handler))
        .with_state(state)
        .merge(
            Router::new()
                .route("/nearby", get(nearby::nearby_handler))
                .route("/places-stats", get(nearby::places_stats_handler))
                .with_state(places),
        )
        .merge(
            Router::new()
                .route("/resolve", get(direct::resolve_handler))
                .route("/guide", get(direct::guide_handler))
                .route("/match", axum::routing::post(direct::match_handler))
                .with_state(direct),
        )
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new());

    // Static assets last, as a fallback: API routes are matched first, and
    // anything unmatched falls through to index.html so client-side routing
    // and deep links work.
    let app = match &args.static_dir {
        Some(dir) => {
            let index = format!("{dir}/index.html");
            tracing::info!("serving frontend from {dir}");
            app.fallback_service(
                ServeDir::new(dir).not_found_service(ServeFile::new(index)),
            )
        }
        None => app,
    };

    let addr = format!("0.0.0.0:{}", args.port);
    tracing::info!("api listening on {addr}");
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app).await?;
    Ok(())
}

/// Sample stored pagerank values to find the corpus scale. Sampling rather
/// than a full scan keeps startup fast on large indexes.
fn measure_median_pagerank(reader: &IndexReader, fields: &Fields) -> f32 {
    let searcher = reader.searcher();
    let mut values = Vec::new();
    'outer: for segment in searcher.segment_readers() {
        let Ok(store) = segment.get_store_reader(0) else { continue };
        for doc_id in 0..segment.max_doc().min(2000) {
            if let Ok(doc) = store.get::<TantivyDocument>(doc_id) {
                if let Some(v) = doc.get_first(fields.pagerank).and_then(|v| v.as_f64()) {
                    values.push(v as f32);
                }
            }
            if values.len() >= 5000 {
                break 'outer;
            }
        }
    }
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// Corpus sizes, so the UI can state plainly what is and isn't indexed.
///
/// A demo that quietly implies web-scale coverage invites the one query that
/// exposes it. Publishing the numbers turns a weakness into a stated scope.
async fn stats_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let web_docs: u64 = state
        .shards
        .iter()
        .map(|s| s.reader.searcher().num_docs())
        .sum();
    Json(serde_json::json!({ "web_documents": web_docs }))
}

async fn search_handler(
    State(state): State<Arc<AppState>>,
    AxumQuery(params): AxumQuery<SearchParams>,
) -> Result<Json<SearchResponse>, AppError> {
    let start = std::time::Instant::now();
    let q = params.q.trim().to_string();
    let limit = params.limit.clamp(1, 50);

    if q.is_empty() {
        return Ok(Json(SearchResponse {
            query: q,
            took_ms: 0,
            total_hits: 0,
            results: Vec::new(),
        }));
    }

    let mut handles = Vec::new();
    for idx in 0..state.shards.len() {
        let state = state.clone();
        let q = q.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            search_shard(&state.shards[idx], &q, limit, &state.weights, params.explain)
        }));
    }

    let mut all = Vec::new();
    let mut total_hits = 0;
    for h in handles {
        match h.await {
            Ok(Ok((mut r, hits))) => {
                all.append(&mut r);
                total_hits += hits;
            }
            Ok(Err(e)) => tracing::warn!("shard search failed: {e:#}"),
            Err(e) => tracing::warn!("shard task panicked: {e}"),
        }
    }
    all.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    all.truncate(limit);

    Ok(Json(SearchResponse {
        query: q,
        took_ms: start.elapsed().as_millis() as u64,
        total_hits,
        results: all,
    }))
}

fn search_shard(
    shard: &Shard,
    q: &str,
    limit: usize,
    weights: &Weights,
    explain: bool,
) -> Result<(Vec<SearchResult>, usize)> {
    let f = &shard.fields;
    let searcher = shard.reader.searcher();

    let mut parser = QueryParser::for_index(
        &shard.index,
        vec![f.title, f.anchor, f.body, f.url_text],
    );
    parser.set_field_boost(f.title, weights.title_boost);
    parser.set_field_boost(f.anchor, weights.anchor_boost);
    parser.set_field_boost(f.url_text, weights.url_boost);
    parser.set_field_boost(f.body, weights.body_boost);
    // Treat unquoted multi-word queries as OR rather than AND: a strict AND
    // returns nothing the moment one term is missing, which on a small
    // corpus means most queries return zero results. Ranking sorts it out.
    parser.set_conjunction_by_default();
    let query: Box<dyn Query> = match parser.parse_query(q) {
        Ok(parsed) => {
            // Retry as OR if the AND interpretation finds nothing.
            let hits = searcher.search(&parsed, &Count)?;
            if hits > 0 {
                parsed
            } else {
                let mut loose = QueryParser::for_index(
                    &shard.index,
                    vec![f.title, f.anchor, f.body, f.url_text],
                );
                loose.set_field_boost(f.title, weights.title_boost);
                loose.set_field_boost(f.anchor, weights.anchor_boost);
                loose.set_field_boost(f.url_text, weights.url_boost);
                loose.parse_query(q).unwrap_or(parsed)
            }
        }
        // Fall back to a literal term query when the input has syntax the
        // parser rejects (stray quotes, colons from a pasted URL, etc).
        Err(_) => {
            let terms: Vec<(Occur, Box<dyn Query>)> = q
                .split_whitespace()
                .map(|t| {
                    let term = tantivy::Term::from_field_text(f.body, &t.to_lowercase());
                    (
                        Occur::Should,
                        Box::new(tantivy::query::TermQuery::new(
                            term,
                            tantivy::schema::IndexRecordOption::WithFreqs,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            Box::new(BooleanQuery::new(terms))
        }
    };

    let total_hits = searcher.search(&query, &Count)?;
    // Over-fetch so post-retrieval re-scoring has room to reorder.
    let top = searcher.search(&query, &TopDocs::with_limit(limit * 5))?;

    let snippet_gen = SnippetGenerator::create(&searcher, &*query, f.body).ok();

    // Normalisation reference: the best text score in this candidate set.
    let max_bm25 = top.iter().map(|(s, _)| *s).fold(0.0f32, f32::max);

    let mut out = Vec::with_capacity(top.len());
    for (bm25, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let signals = rank::Signals {
            bm25,
            pagerank: doc.get_first(f.pagerank).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
            quality: doc.get_first(f.quality).and_then(|v| v.as_f64()).unwrap_or(1.0) as f32,
            inbound_domains: doc
                .get_first(f.inbound_domains)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
        };
        let scored = rank::score(&signals, weights, shard.median_pagerank, max_bm25);

        let snippet = snippet_gen
            .as_ref()
            .map(|g| g.snippet_from_doc(&doc).to_html())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                doc.get_first(f.body)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(180)
                    .collect()
            });

        out.push(SearchResult {
            url: doc.get_first(f.url).and_then(|v| v.as_str()).unwrap_or("").to_string(),
            title: doc.get_first(f.title).and_then(|v| v.as_str()).unwrap_or("").to_string(),
            snippet,
            score: scored.score,
            explain: explain.then_some(scored.explain),
        });
    }

    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit);
    Ok((out, total_hits))
}
