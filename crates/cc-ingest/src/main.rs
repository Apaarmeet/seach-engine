mod cdx;
mod warc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use common::shard_for_url;
use futures::stream::{self, StreamExt};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Ingest a slice of Common Crawl into the same JSONL format the live
/// crawler produces, so `ranker`, `build-index` and `api` work unchanged.
///
/// Why this exists: crawling the Indian web from scratch means discovering
/// and fetching millions of pages yourself. Common Crawl already did it.
/// Because the CDX index is SURT-sorted, an entire ccTLD is a contiguous
/// range — so pulling all of `.in` costs a range scan plus ~390GB of
/// byte-range fetches, not the ~100TB of a full crawl.
#[derive(Parser, Debug)]
#[command(about = "Ingest Common Crawl slices into the search index")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Estimate size of a slice without downloading page content.
    Estimate(SliceArgs),
    /// Download and extract pages into data/crawled/*.jsonl.
    Fetch {
        #[command(flatten)]
        slice: SliceArgs,
        /// Stop after this many successfully extracted pages. 0 = no limit.
        #[arg(long, default_value_t = 0)]
        max_pages: usize,
        /// Concurrent WARC record fetches.
        #[arg(long, default_value_t = 32)]
        concurrency: usize,
        #[arg(long, default_value = "data/crawled")]
        out_dir: String,
    },
}

#[derive(Parser, Debug, Clone)]
struct SliceArgs {
    /// Common Crawl snapshot id.
    #[arg(long, default_value = "CC-MAIN-2026-34")]
    crawl: String,
    /// SURT prefix to select; repeat the flag for several. `in,` selects the
    /// entire .in ccTLD; `com,flipkart)` selects one Indian .com site.
    ///
    /// Deliberately NOT comma-delimited: SURT keys themselves contain
    /// commas, so a comma delimiter would split `in,` into `["in", ""]` and
    /// the empty prefix matches every block in the crawl.
    #[arg(long = "prefix", default_values_t = [String::from("in,")])]
    prefixes: Vec<String>,
    /// Cache path for the ~100MB cluster.idx.
    #[arg(long, default_value = "data/warc-cache/cluster.idx")]
    cluster_cache: String,
    /// Limit how many CDX blocks to process (useful for sampling).
    #[arg(long, default_value_t = 0)]
    max_blocks: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let client = reqwest::Client::builder()
        .user_agent("rsearch-cc-ingest/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    match args.cmd {
        Cmd::Estimate(slice) => estimate(&client, &slice).await,
        Cmd::Fetch { slice, max_pages, concurrency, out_dir } => {
            fetch(&client, &slice, max_pages, concurrency, &out_dir).await
        }
    }
}

async fn load_cluster_idx(client: &reqwest::Client, slice: &SliceArgs) -> Result<String> {
    if let Ok(cached) = std::fs::read_to_string(&slice.cluster_cache) {
        tracing::info!("using cached cluster.idx ({} bytes)", cached.len());
        return Ok(cached);
    }
    let url = cdx::cluster_idx_url(&slice.crawl);
    tracing::info!("downloading {url} (~100MB, cached after first run)");
    let body = client.get(&url).send().await?.error_for_status()?.text().await?;
    if let Some(parent) = std::path::Path::new(&slice.cluster_cache).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&slice.cluster_cache, &body).context("caching cluster.idx")?;
    Ok(body)
}

async fn estimate(client: &reqwest::Client, slice: &SliceArgs) -> Result<()> {
    let cluster = load_cluster_idx(client, slice).await?;
    let total_blocks = cluster.lines().count();
    let blocks = cdx::select_blocks(&cluster, &slice.prefixes);
    let cdx_bytes: u64 = blocks.iter().map(|b| b.length).sum();

    // Sample one block from the middle to measure real record sizes rather
    // than guessing an average.
    let sample = blocks.get(blocks.len() / 2);
    let (pages_per_block, avg_record, ok_ratio) = match sample {
        Some(block) => {
            let recs = cdx::fetch_block(client, &slice.crawl, block, &slice.prefixes).await?;
            let n = recs.len().max(1);
            let fetchable: Vec<_> = recs.iter().filter(|r| r.is_fetchable_html()).collect();
            let total: u64 = fetchable
                .iter()
                .filter_map(|r| r.warc_range().map(|(_, l)| l))
                .sum();
            let avg = if fetchable.is_empty() { 0 } else { total / fetchable.len() as u64 };
            (n, avg, fetchable.len() as f64 / n as f64)
        }
        None => (0, 0, 0.0),
    };

    let est_pages = blocks.len() * pages_per_block;
    let est_fetchable = (est_pages as f64 * ok_ratio) as u64;
    let est_bytes = est_fetchable * avg_record;

    println!("crawl              {}", slice.crawl);
    println!("prefixes           {:?}", slice.prefixes);
    println!("blocks matched     {} of {} ({:.2}% of crawl)",
        blocks.len(), total_blocks,
        100.0 * blocks.len() as f64 / total_blocks as f64);
    println!("cdx index size     {:.2} GB", cdx_bytes as f64 / 1e9);
    println!("pages (all)        {:.1}M", est_pages as f64 / 1e6);
    println!("pages (200 + html) {:.1}M  ({:.0}% of captures)", est_fetchable as f64 / 1e6, ok_ratio * 100.0);
    println!("avg record         {:.1} KB", avg_record as f64 / 1024.0);
    println!("download           {:.2} TB", est_bytes as f64 / 1e12);
    Ok(())
}

async fn fetch(
    client: &reqwest::Client,
    slice: &SliceArgs,
    max_pages: usize,
    concurrency: usize,
    out_dir: &str,
) -> Result<()> {
    let cluster = load_cluster_idx(client, slice).await?;
    let mut blocks = cdx::select_blocks(&cluster, &slice.prefixes);
    if slice.max_blocks > 0 {
        blocks.truncate(slice.max_blocks);
    }
    tracing::info!("{} cdx blocks to process", blocks.len());
    std::fs::create_dir_all(out_dir)?;

    let written = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));

    // One append-only handle per shard, so output drops straight into the
    // layout `build-index` already reads.
    let out_path = format!("{out_dir}/cc-shard-0.jsonl");
    let file = Arc::new(std::sync::Mutex::new(
        OpenOptions::new().create(true).append(true).open(&out_path)?,
    ));

    'outer: for (i, block) in blocks.iter().enumerate() {
        let records = match cdx::fetch_block(client, &slice.crawl, block, &slice.prefixes).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("block {} failed: {e}", block.file);
                continue;
            }
        };
        let fetchable: Vec<_> = records.into_iter().filter(|r| r.is_fetchable_html()).collect();

        let results = stream::iter(fetchable)
            .map(|rec| {
                let client = client.clone();
                async move {
                    let (offset, length) = rec.warc_range()?;
                    warc::fetch_and_parse(&client, &rec.filename, offset, length, &rec.url)
                        .await
                        .ok()
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;

        for page in results {
            match page {
                Some(page) if !page.body_text.is_empty() => {
                    let line = serde_json::to_string(&page)?;
                    // shard_for_url is computed here so a multi-machine run
                    // can route pages without re-parsing them later.
                    let _shard = shard_for_url(&page.url, 1);
                    let mut f = file.lock().unwrap();
                    writeln!(f, "{line}")?;
                    let n = written.fetch_add(1, Ordering::Relaxed) + 1;
                    if max_pages > 0 && n as usize >= max_pages {
                        tracing::info!("hit max_pages limit");
                        break 'outer;
                    }
                }
                _ => {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if i % 10 == 0 {
            tracing::info!(
                "block {}/{} | written {} | failed {}",
                i + 1,
                blocks.len(),
                written.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed)
            );
        }
    }

    tracing::info!(
        "done: {} pages -> {out_path} ({} failed)",
        written.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed)
    );
    Ok(())
}
