//! CLI for the URL resolver: `cargo run -p resolver --bin resolve -- "<query>"`.
//!
//! Exists so the pipeline can be exercised against the live network without
//! starting the API, and so the funnel (guessed -> resolves -> confirmed) is
//! visible while tuning.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("resolver=debug,warn").init();

    let query: Vec<String> = std::env::args().skip(1).collect();
    if query.is_empty() {
        eprintln!("usage: resolve \"<query>\"");
        std::process::exit(2);
    }
    let query = query.join(" ");

    let gaz = places::gazetteer::Gazetteer::load("places-index/gazetteer.json");
    eprintln!("gazetteer: {} place names", gaz.len());

    let is_place = resolver::place_gate(&gaz);
    let kinds: Vec<String> = resolver::query::parse(&query, &is_place)
        .tokens
        .iter()
        .map(|t| format!("{}={:?}", t.text, t.kind))
        .collect();
    eprintln!("tokens: {}", kinds.join(" "));

    let client = resolver::probe::http_client()?;
    let started = std::time::Instant::now();
    let r = resolver::resolve(
        &client,
        &query,
        &gaz,
        &resolver::Sources {
            directory: None,
            wikidata: true,
            web: resolver::websearch::Provider::from_env(),
            llm: resolver::llm::Llm::from_env(),
        },
    )
    .await;
    let elapsed = started.elapsed();

    println!("\nquery: {}", r.query);
    println!(
        "funnel: {} guessed -> {} resolve in DNS -> {} fetched  [{:?}]",
        r.trace.guessed, r.trace.resolved, r.trace.fetched, elapsed
    );
    println!(
        "stages: dns {}ms, fetch {}ms, {} sitemap urls",
        r.trace.dns_ms, r.trace.fetch_ms, r.trace.sitemap_urls
    );
    println!("suffixes tried: {}", r.trace.suffixes.join(", "));
    if !r.trace.live_hosts.is_empty() {
        println!("live: {}", r.trace.live_hosts.join(", "));
    }

    if let Some(a) = &r.answer {
        println!("\nanswer: {}", a.text);
        println!("   — {}", a.source_url);
    }

    if r.answers.is_empty() {
        println!("\nno confirmed answer");
    }
    for (i, a) in r.answers.iter().enumerate() {
        println!("\n{}. {}  [{:.2}]", i + 1, a.url, a.score);
        println!("   {}", a.title);
        println!("   quality {:.2} — {}", a.quality, a.via);
    }
    Ok(())
}
