use anyhow::Result;
use clap::Parser;
use common::{CrawledPage, PageSignals};
use std::collections::HashMap;
use std::io::BufRead;
use tantivy::doc;

/// Build a tantivy index shard from crawled pages plus the offline signals.
#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "data/crawled")]
    crawled_dir: String,
    #[arg(long, default_value = "index")]
    index_dir: String,
    /// Output of the `signals` pass.
    #[arg(long, default_value = "data/signals.jsonl")]
    signals_file: Option<String>,
    /// Drop pages the signals pass flagged as near-duplicates.
    #[arg(long, default_value_t = true)]
    drop_duplicates: bool,
    /// Drop pages scoring below this quality threshold (0 disables).
    #[arg(long, default_value_t = 0.15)]
    min_quality: f32,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mut signals: HashMap<String, PageSignals> = HashMap::new();
    if let Some(path) = &args.signals_file {
        match std::fs::File::open(path) {
            Ok(file) => {
                for line in std::io::BufReader::new(file).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    let s: PageSignals = serde_json::from_str(&line)?;
                    signals.insert(s.url.clone(), s);
                }
                tracing::info!("loaded {} signal records", signals.len());
            }
            Err(e) => tracing::warn!("no signals file at {path} ({e}); indexing text only"),
        }
    }

    let (index, f) = indexer::open_or_create_index(&args.index_dir)?;
    let mut writer = index.writer(200_000_000)?;
    writer.delete_all_documents()?;

    let (mut indexed, mut skipped_dupe, mut skipped_junk) = (0usize, 0usize, 0usize);

    for entry in std::fs::read_dir(&args.crawled_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file = std::fs::File::open(&path)?;
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let page: CrawledPage = match serde_json::from_str(&line) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("skipping malformed line: {e}");
                    continue;
                }
            };
            if page.status != 200 {
                continue;
            }

            let sig = signals.get(&page.url);
            if args.drop_duplicates && sig.map(|s| s.is_duplicate).unwrap_or(false) {
                skipped_dupe += 1;
                continue;
            }
            let quality = sig.map(|s| s.quality).unwrap_or(1.0);
            if quality < args.min_quality {
                skipped_junk += 1;
                continue;
            }

            writer.add_document(doc!(
                f.url             => page.url.clone(),
                f.url_text        => url_to_text(&page.url),
                f.title           => page.title,
                f.anchor          => sig.map(|s| s.anchor_text.clone()).unwrap_or_default(),
                f.body            => page.body_text,
                f.pagerank        => sig.map(|s| s.pagerank).unwrap_or(0.0) as f64,
                f.quality         => quality as f64,
                f.inbound_domains => sig.map(|s| s.inbound_domains).unwrap_or(0) as u64,
            ))?;
            indexed += 1;
        }
    }

    writer.commit()?;
    tracing::info!(
        "indexed {indexed} docs into {} (skipped {skipped_dupe} duplicates, {skipped_junk} low-quality)",
        args.index_dir
    );
    Ok(())
}

/// Turn a URL into searchable words so "irctc" matches `https://irctc.co.in/`.
/// Domain-name matches are a strong navigational signal.
fn url_to_text(url: &str) -> String {
    url.replace(['/', '.', '-', '_', '?', '&', '=', ':'], " ")
        .split_whitespace()
        .filter(|t| !matches!(*t, "https" | "http" | "www" | "html" | "php" | "index"))
        .collect::<Vec<_>>()
        .join(" ")
}
