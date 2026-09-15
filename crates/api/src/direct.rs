//! `/resolve` — the navigational endpoint.
//!
//! Kept off `/search` deliberately, and the reason is latency shape rather
//! than tidiness. Index search is a local mmap read measured in
//! milliseconds; resolution is DNS plus live HTTP against third-party hosts,
//! measured in seconds. Folding the second into the first would make every
//! query as slow as the slowest stranger's web server.
//!
//! So the two run side by side: the UI paints index results immediately and
//! fills in the resolved URL when it arrives. The user sees a fast page that
//! gets better, instead of a blank one that eventually appears.

use crate::AppError;
use axum::extract::{Query as AxumQuery, State};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

/// How long a resolved answer stays good.
///
/// Navigational answers are extremely stable — a school's website does not
/// change between two people asking for it — so this can be generous.
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Most queries held at once.
const CACHE_CAPACITY: usize = 500;

pub struct DirectState {
    pub client: reqwest::Client,
    pub places: Arc<crate::nearby::PlacesShard>,
    /// Query -> answer, with the time it was computed.
    ///
    /// Not just a speed optimisation — it is what makes the endpoint
    /// *deterministic*. Resolution races a dozen live fetches against
    /// third-party servers on a short timeout, so which candidates survive
    /// depends on network weather: the same query returned
    /// `stedmundshillong.in` on one run and the Wikipedia article on the
    /// next, and returned nothing at all for "valuepickr bajaj finance" once
    /// out of two. A search box that answers differently each time reads as
    /// broken no matter how good the average is.
    pub cache: tokio::sync::Mutex<
        std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>,
    >,
    /// Configured from the environment at startup. Absent is a supported
    /// configuration: the resolver runs on its self-contained sources alone.
    pub web: Option<resolver::websearch::Provider>,
    /// Language model for control selection and inline answers.
    pub llm: Option<resolver::llm::Llm>,
}

/// The places index as an entity -> website directory.
///
/// This is the payoff for having ingested OpenStreetMap. The `website` tag
/// on a POI is a *recorded* fact about an institution, which is exactly the
/// information hostname guessing lacks: nothing about the words "panjab
/// university" implies `puchd.ac.in`, and nothing ever will. OSM simply
/// knows.
///
/// Coverage is partial and that is fine. Every entry it does have is one the
/// guesser could not reach, and the guesser still covers everything the
/// directory misses. They fail in different directions, which is the only
/// reason it is worth running both.
struct OsmDirectory<'a>(&'a crate::nearby::PlacesShard);

impl resolver::Directory for OsmDirectory<'_> {
    fn lookup(&self, query: &str, limit: usize) -> Vec<resolver::DirectoryHit> {
        use tantivy::collector::TopDocs;
        use tantivy::query::QueryParser;
        use tantivy::schema::Value;

        let f = &self.0.fields;
        let searcher = self.0.reader.searcher();
        // Name and brand only. Matching on category would return every
        // school in the country for the word "school".
        let mut parser =
            QueryParser::for_index(&self.0.index, vec![f.name, f.brand]);
        parser.set_conjunction_by_default();

        // Conjunction only, with no OR fallback.
        //
        // A directory hit is inserted ahead of every guess, so a loose match
        // here is expensive: asked for "chandigarh university bca", an OR
        // query matched any POI containing "chandigarh" *or* "university"
        // and put four unrelated institutions' websites at the front of the
        // queue. Returning nothing is the right answer when the directory
        // does not actually know the entity — the guesser is still running.
        let parsed = match parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };
        let docs = searcher
            .search(&parsed, &TopDocs::with_limit(limit * 4))
            .unwrap_or_default();

        let mut out = Vec::new();
        for (_score, addr) in docs {
            let Ok(doc) = searcher.doc::<tantivy::TantivyDocument>(addr) else {
                continue;
            };
            let website = doc
                .get_first(f.website)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if website.is_empty() || !website.starts_with("http") {
                continue;
            }
            let name = doc
                .get_first(f.name)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            out.push(resolver::DirectoryHit {
                name,
                website,
                source: "openstreetmap".into(),
            });
            if out.len() >= limit {
                break;
            }
        }
        out
    }
}

pub fn open(places: Arc<crate::nearby::PlacesShard>) -> anyhow::Result<DirectState> {
    let web = resolver::websearch::Provider::from_env();
    match &web {
        Some(p) => tracing::info!("web index source: {}", p.name()),
        None => tracing::info!(
            "no web index configured (set SERPER_API_KEY, GOOGLE_CSE_KEY+CX, \
             SEARXNG_URL or BRAVE_API_KEY) — resolving from own sources only"
        ),
    }
    let llm = resolver::llm::Llm::from_env();
    match &llm {
        Some(m) => tracing::info!("llm enabled: {}", m.model()),
        None => tracing::info!("OPENROUTER_API_KEY not set — heuristic control matching only"),
    }
    Ok(DirectState {
        client: resolver::probe::http_client()?,
        places,
        web,
        llm,
        cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    })
}

#[derive(Deserialize)]
pub struct MatchRequest {
    /// The control the step asked for, in the model's words.
    pub find: String,
    #[serde(default)]
    pub instruction: String,
    /// Visible text of every control actually on the rendered page.
    pub controls: Vec<String>,
}

/// `/match` — map a planned step onto a control that really exists.
///
/// The step plan is written from a *help page*, and help pages do not use the
/// same words as the product. Cloudflare's docs say "log in"; the button on
/// the login screen says **Sign in**. Literal matching finds nothing, the
/// walkthrough goes silent, and the user is left on a page with no guidance
/// at exactly the moment they needed it.
///
/// So when the extension cannot find the control itself, it sends the list of
/// controls that *are* on the page and asks which one the step meant. This is
/// the right place for the model to work: on the rendered DOM, against real
/// options, choosing rather than inventing. It also dissolves the whole class
/// of failure where served markup differs from what the browser displays —
/// region-swapped labels, client-rendered text, the lot.
pub async fn match_handler(
    State(state): State<Arc<DirectState>>,
    Json(req): Json<MatchRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let Some(model) = &state.llm else {
        return Ok(Json(serde_json::json!({ "match": null })));
    };
    if req.controls.is_empty() {
        return Ok(Json(serde_json::json!({ "match": null })));
    }

    let request = if req.instruction.is_empty() {
        req.find.clone()
    } else {
        format!("{} ({})", req.find, req.instruction)
    };

    // Capped: a dense app can expose hundreds of controls, and the prompt has
    // to stay small enough to answer quickly on the page's critical path.
    let controls: Vec<String> = req.controls.into_iter().take(80).collect();
    let chosen = model
        .choose_control(&state.client, &request, "", &controls)
        .await;

    Ok(Json(serde_json::json!({ "match": chosen })))
}

/// `/guide` — a click-by-click walkthrough for a task.
///
/// Separate from `/resolve` because it answers a different question: not
/// "where do I go" but "what do I do when I get there". The browser
/// extension drives it, spotlighting one control at a time.
pub async fn guide_handler(
    State(state): State<Arc<DirectState>>,
    AxumQuery(params): AxumQuery<ResolveParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let q = params.q.trim();
    if q.is_empty() {
        return Err(AppError::bad_request("q must not be empty"));
    }
    let Some(model) = &state.llm else {
        return Err(AppError::bad_request(
            "guidance needs a language model; set OPENROUTER_API_KEY",
        ));
    };

    let directory = OsmDirectory(&state.places);
    let r = resolver::resolve(
        &state.client,
        q,
        &state.places.gazetteer,
        &resolver::Sources {
            directory: Some(&directory),
            wikidata: true,
            web: state.web.clone(),
            llm: state.llm.clone(),
        },
    )
    .await;

    let Some(top) = r.answers.first() else {
        return Ok(Json(serde_json::json!({ "query": q, "steps": [] })));
    };

    let plan = model
        .plan_steps(&state.client, q, &top.title, &top.body_text)
        .await;

    // The product's own page wins over the help article.
    //
    // "How do I cancel my YouTube plan" is a request to *cancel*, not to read
    // about cancelling — so the destination is
    // `youtube.com/paid_memberships`, which the support page names in its own
    // instructions, and the article is only where the steps came from. Where
    // the page names no such URL, the article is still a reasonable place to
    // land.
    let start_url = if plan.start_url.starts_with("http") {
        plan.start_url.clone()
    } else {
        top.url.clone()
    };

    Ok(Json(serde_json::json!({
        "query": q,
        "start_url": start_url,
        "source_url": top.url,
        "title": top.title,
        "steps": plan.steps,
    })))
}

#[derive(Deserialize)]
pub struct ResolveParams {
    pub q: String,
    /// Include the funnel — how many hostnames were guessed, how many exist,
    /// how many were retrieved. This is the explanation of the design, so it
    /// is a first-class response field rather than a log line.
    #[serde(default)]
    pub explain: bool,
}

pub async fn resolve_handler(
    State(state): State<Arc<DirectState>>,
    AxumQuery(params): AxumQuery<ResolveParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let q = params.q.trim();
    if q.is_empty() {
        return Err(AppError::bad_request("q must not be empty"));
    }
    if q.len() > 200 {
        // The candidate generator is combinatorial in token count. A long
        // query is not a navigational query, and refusing it is cheaper than
        // discovering that at the DNS stage.
        return Err(AppError::bad_request("q is too long to resolve"));
    }

    let started = std::time::Instant::now();

    // Normalised so "St Edmunds  School" and "st edmunds school" share an
    // entry; resolution is already case- and whitespace-insensitive.
    let key = q.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    {
        let cache = state.cache.lock().await;
        if let Some((at, body)) = cache.get(&key) {
            if at.elapsed() < CACHE_TTL {
                let mut body = body.clone();
                body["cached"] = serde_json::Value::Bool(true);
                body["took_ms"] = serde_json::json!(started.elapsed().as_millis() as u64);
                return Ok(Json(body));
            }
        }
    }

    let directory = OsmDirectory(&state.places);
    let r = resolver::resolve(
        &state.client,
        q,
        &state.places.gazetteer,
        &resolver::Sources {
            directory: Some(&directory),
            wikidata: true,
            web: state.web.clone(),
            llm: state.llm.clone(),
        },
    )
    .await;

    let mut body = serde_json::json!({
        "query": r.query,
        "took_ms": started.elapsed().as_millis() as u64,
        "answers": r.answers,
    });
    if let Some(a) = &r.answer {
        body["answer"] = serde_json::to_value(a).unwrap_or(serde_json::Value::Null);
    }
    if params.explain {
        body["trace"] = serde_json::to_value(&r.trace).unwrap_or(serde_json::Value::Null);
    }

    // Only cache a confident answer. Caching a miss would freeze a transient
    // network failure in place for an hour, which is the opposite of the
    // stability this exists to provide.
    if !r.answers.is_empty() {
        let mut cache = state.cache.lock().await;
        if cache.len() >= CACHE_CAPACITY {
            // Crude eviction: drop everything expired, and if that frees
            // nothing, start over. A navigational cache is cheap to rebuild.
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= CACHE_CAPACITY {
                cache.clear();
            }
        }
        cache.insert(key, (std::time::Instant::now(), body.clone()));
    }

    Ok(Json(body))
}
