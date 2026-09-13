mod metrics;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::BufRead;

/// Measure ranking quality against a judgment set.
///
/// Workflow this enables: change a weight in `rank.rs`, re-run this, see
/// whether NDCG@10 moved. Without it, "improving relevance" is vibes.
#[derive(Parser, Debug)]
struct Args {
    /// JSONL judgments: {"query": "...", "judgments": {"<url>": 0-3}}
    #[arg(long, default_value = "eval/judgments.jsonl")]
    judgments: String,
    #[arg(long, default_value = "http://localhost:8080")]
    api: String,
    #[arg(long, default_value_t = 10)]
    k: usize,
    /// Print the ranked list for each query, not just the totals.
    #[arg(long)]
    verbose: bool,
}

/// Graded relevance, the TREC convention:
///   3 = the answer, 2 = very useful, 1 = marginal, 0 = irrelevant.
/// Graded (not binary) judgments are what make NDCG meaningful.
#[derive(Debug, Deserialize)]
struct Judged {
    query: String,
    judgments: HashMap<String, f32>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    results: Vec<ApiResult>,
    took_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ApiResult {
    url: String,
    title: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let file = std::fs::File::open(&args.judgments)
        .with_context(|| format!("opening {}", args.judgments))?;
    let mut cases = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() || line.trim_start().starts_with("//") {
            continue;
        }
        cases.push(serde_json::from_str::<Judged>(&line)?);
    }
    if cases.is_empty() {
        anyhow::bail!("no judgments found in {}", args.judgments);
    }

    let client = reqwest::Client::new();
    let (mut sum_ndcg, mut sum_rr, mut sum_p, mut latency) = (0.0, 0.0, 0.0, 0u64);

    println!("{:<34} {:>8} {:>8} {:>8}", "QUERY", "NDCG@10", "RR", "P@10");
    println!("{}", "-".repeat(62));

    for case in &cases {
        let url = format!("{}/search", args.api);
        let resp: ApiResponse = client
            .get(&url)
            .query(&[("q", case.query.as_str()), ("limit", &args.k.to_string())])
            .send()
            .await
            .with_context(|| format!("querying api for {:?}", case.query))?
            .json()
            .await?;
        latency += resp.took_ms;

        // Gain of each returned result, in rank order.
        let gains: Vec<f32> = resp
            .results
            .iter()
            .map(|r| *case.judgments.get(&r.url).unwrap_or(&0.0))
            .collect();

        // Best possible ordering of everything we judged relevant.
        let mut ideal: Vec<f32> = case.judgments.values().copied().collect();
        ideal.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

        let ndcg = metrics::ndcg_at_k(&gains, &ideal, args.k);
        let rr = metrics::reciprocal_rank(&gains, 2.0);
        let p = metrics::precision_at_k(&gains, args.k, 2.0);
        sum_ndcg += ndcg;
        sum_rr += rr;
        sum_p += p;

        let flag = if ndcg < 0.5 { " <-- weak" } else { "" };
        println!("{:<34} {:>8.3} {:>8.3} {:>8.3}{flag}", truncate(&case.query, 33), ndcg, rr, p);

        if args.verbose {
            for (i, r) in resp.results.iter().enumerate() {
                let g = case.judgments.get(&r.url).copied().unwrap_or(0.0);
                let mark = match g {
                    g if g >= 3.0 => "***",
                    g if g >= 2.0 => "** ",
                    g if g >= 1.0 => "*  ",
                    _ => "   ",
                };
                println!("     {mark} {}. {} — {}", i + 1, truncate(&r.title, 44), truncate(&r.url, 46));
            }
        }
    }

    let n = cases.len() as f32;
    println!("{}", "-".repeat(62));
    println!("{:<34} {:>8.3} {:>8.3} {:>8.3}", "MEAN", sum_ndcg / n, sum_rr / n, sum_p / n);
    println!("\n{} queries, mean latency {:.1}ms", cases.len(), latency as f32 / n);
    println!("\nNDCG@{}: 1.0 is a perfect ranking. Track this number across", args.k);
    println!("ranking changes — if it drops, revert regardless of how the");
    println!("change felt on your favourite query.");
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}
