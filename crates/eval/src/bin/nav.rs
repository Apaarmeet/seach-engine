//! Measure the URL resolver against a navigational judgment set.
//!
//! The sibling `eval` binary measures ranking with NDCG, which assumes
//! degrees of rightness. Navigational search has none: the user wants one
//! page, and the ninth-best result is not partial credit, it is a failure.
//! So this reports success@1 — did the exact URL come back first.
//!
//! It also splits the failures, which is the number that should drive
//! decisions. Returning nothing and returning the wrong site are both
//! misses, but they are not equally bad: a blank result makes the user try
//! again, while a confident wrong URL makes them click it and conclude the
//! product is broken. A change that converts silence into wrong answers can
//! improve success@1 and still make the product worse, and only a split
//! report shows that.
//!
//!   cargo run -p eval --bin nav -- --api http://localhost:8080

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::BufRead;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "eval/navigational.jsonl")]
    judgments: String,
    #[arg(long, default_value = "http://localhost:8080")]
    api: String,
    /// Print every query's outcome, not just the summary.
    #[arg(long, default_value_t = true)]
    verbose: bool,
    /// Only run cases of this kind (institution, site-page, misspelled, ...).
    #[arg(long)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Case {
    query: String,
    url: String,
    #[serde(default)]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct ResolveResponse {
    #[serde(default)]
    answers: Vec<Answer>,
    #[serde(default)]
    took_ms: u64,
}

#[derive(Debug, Deserialize)]
struct Answer {
    url: String,
    #[serde(default)]
    score: f32,
}

/// What happened on one query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Gold URL returned first.
    Hit,
    /// Gold URL returned, but not first.
    HitAtK,
    /// Answered, and the top answer is not the gold URL.
    Wrong,
    /// Declined to answer.
    Silent,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Hit => "HIT",
            Outcome::HitAtK => "hit@3",
            Outcome::Wrong => "WRONG",
            Outcome::Silent => "silent",
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let file = std::fs::File::open(&args.judgments)
        .with_context(|| format!("opening {}", args.judgments))?;
    let mut cases: Vec<Case> = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        cases.push(
            serde_json::from_str(t).with_context(|| format!("parsing {t}"))?,
        );
    }
    if let Some(kind) = &args.kind {
        cases.retain(|c| c.kind == *kind);
    }
    if cases.is_empty() {
        anyhow::bail!("no cases to run");
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()?;

    let mut outcomes: Vec<(Case, Outcome, u64, Option<String>)> = Vec::new();

    println!("{:<38} {:>8} {:>7}  {}", "QUERY", "OUTCOME", "ms", "TOP ANSWER");
    println!("{}", "-".repeat(100));

    for case in cases {
        let url = format!("{}/resolve", args.api);
        let resp = client.get(&url).query(&[("q", &case.query)]).send().await;

        let (outcome, took, top) = match resp {
            Ok(r) if r.status().is_success() => {
                let body: ResolveResponse = r.json().await.unwrap_or(ResolveResponse {
                    answers: Vec::new(),
                    took_ms: 0,
                });
                let gold = common::canonical_url(&case.url);
                let top = body.answers.first().map(|a| a.url.clone());
                let outcome = if body.answers.is_empty() {
                    Outcome::Silent
                } else if common::canonical_url(&body.answers[0].url) == gold {
                    Outcome::Hit
                } else if body
                    .answers
                    .iter()
                    .take(3)
                    .any(|a| common::canonical_url(&a.url) == gold)
                {
                    Outcome::HitAtK
                } else {
                    Outcome::Wrong
                };
                (outcome, body.took_ms, top)
            }
            Ok(r) => {
                eprintln!("{}: HTTP {}", case.query, r.status());
                (Outcome::Silent, 0, None)
            }
            Err(e) => {
                eprintln!("{}: {e}", case.query);
                (Outcome::Silent, 0, None)
            }
        };

        if args.verbose {
            println!(
                "{:<38} {:>8} {:>7}  {}",
                truncate(&case.query, 38),
                outcome.label(),
                took,
                top.clone().unwrap_or_else(|| "-".into())
            );
        }
        outcomes.push((case, outcome, took, top));
    }

    report(&outcomes);
    Ok(())
}

fn report(outcomes: &[(Case, Outcome, u64, Option<String>)]) {
    let n = outcomes.len() as f32;
    let count = |o: Outcome| outcomes.iter().filter(|(_, x, _, _)| *x == o).count();

    let hit = count(Outcome::Hit);
    let at_k = count(Outcome::HitAtK);
    let wrong = count(Outcome::Wrong);
    let silent = count(Outcome::Silent);

    let mut latencies: Vec<u64> =
        outcomes.iter().filter(|(_, _, ms, _)| *ms > 0).map(|(_, _, ms, _)| *ms).collect();
    latencies.sort_unstable();
    let median = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
    let p90 = latencies
        .get((latencies.len() as f32 * 0.9) as usize)
        .copied()
        .unwrap_or(0);

    println!("\n{}", "=".repeat(60));
    println!("{:<26} {:>6} {:>8}", "", "COUNT", "SHARE");
    println!("{:<26} {:>6} {:>7.0}%", "success@1", hit, 100.0 * hit as f32 / n);
    println!(
        "{:<26} {:>6} {:>7.0}%",
        "success@3",
        hit + at_k,
        100.0 * (hit + at_k) as f32 / n
    );
    println!(
        "{:<26} {:>6} {:>7.0}%",
        "wrong answer (costly)",
        wrong,
        100.0 * wrong as f32 / n
    );
    println!(
        "{:<26} {:>6} {:>7.0}%",
        "no answer (safe)",
        silent,
        100.0 * silent as f32 / n
    );
    println!("\nlatency: median {median} ms, p90 {p90} ms   (n = {})", outcomes.len());

    // By kind, because the shapes fail for different reasons and an average
    // over all of them hides which part needs work.
    let mut by_kind: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (case, outcome, _, _) in outcomes {
        let e = by_kind.entry(case.kind.as_str()).or_insert((0, 0));
        e.1 += 1;
        if *outcome == Outcome::Hit {
            e.0 += 1;
        }
    }
    println!("\n{:<20} {:>10}", "KIND", "success@1");
    for (kind, (hits, total)) in by_kind {
        println!("{:<20} {:>4}/{:<4} {:>3.0}%", kind, hits, total, 100.0 * hits as f32 / total as f32);
    }

    if wrong > 0 {
        println!("\nwrong answers — the failures that cost trust:");
        for (case, outcome, _, top) in outcomes {
            if *outcome == Outcome::Wrong {
                println!("  {:<34} got {}", truncate(&case.query, 34), top.clone().unwrap_or_default());
                println!("  {:<34} want {}", "", case.url);
            }
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
