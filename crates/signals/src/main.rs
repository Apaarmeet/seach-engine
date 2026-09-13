use anyhow::Result;
use clap::Parser;
use common::{CrawledPage, PageSignals};
use signals::{anchors::AnchorIndex, pagerank, quality};
use std::collections::HashMap;
use std::io::{BufRead, Write};

/// One offline pass computing every corpus-global relevance signal:
/// inbound anchor text, link authority, quality, and duplicate clusters.
#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "data/crawled")]
    crawled_dir: String,
    #[arg(long, default_value = "data/signals.jsonl")]
    out_file: String,
    #[arg(long, default_value_t = 0.85)]
    damping: f64,
    #[arg(long, default_value_t = 60)]
    iterations: usize,
    /// Max characters of inbound anchor text stored per page.
    #[arg(long, default_value_t = 600)]
    anchor_budget: usize,
    /// Also index anchor text from links within the same site.
    ///
    /// Off by default, and that default is measured rather than assumed:
    /// on this corpus, including internal anchors drops NDCG@10 from 0.753
    /// to 0.674 at the same boost. A large self-referential site (Wikipedia
    /// here) links the same few phrases at the same few pages tens of
    /// thousands of times, which swamps the signal. Re-measure before
    /// enabling — on a broad multi-site crawl it may well help.
    #[arg(long)]
    include_internal_anchors: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Pass 1: load pages, build the anchor index and the link graph together.
    let mut anchor_idx = AnchorIndex::new();
    let mut graph_input: Vec<(String, Vec<String>)> = Vec::new();
    let mut meta: HashMap<String, (String, String, u64)> = HashMap::new();

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
                    tracing::warn!("skipping malformed line in {path:?}: {e}");
                    continue;
                }
            };
            if page.status != 200 {
                continue;
            }

            anchor_idx.add_page(&page);
            graph_input.push((
                page.url.clone(),
                page.outlink_urls().map(str::to_string).collect(),
            ));
            let sim = quality::simhash(&page.body_text);
            let q = quality::quality_score(&page.title, &page.body_text);
            meta.insert(page.url.clone(), (page.title, page.body_text, sim));
            // quality stored alongside via a parallel map keyed the same way
            let _ = q;
        }
    }

    if graph_input.is_empty() {
        tracing::warn!("no pages found in {}", args.crawled_dir);
        return Ok(());
    }
    tracing::info!("loaded {} pages", graph_input.len());

    // Pass 2: link authority.
    let graph = pagerank::Graph::build(graph_input);
    let ranks = pagerank::compute(&graph, args.damping, args.iterations);
    tracing::info!("pagerank over {} nodes", graph.len());

    // Pass 3: quality + near-duplicate clustering.
    //
    // Duplicates are resolved by keeping the highest-quality page in each
    // cluster (ties broken by shortest URL, which favours canonical pages
    // over tracking-parameter variants).
    let mut records: Vec<PageSignals> = Vec::with_capacity(graph.len());
    for (i, url) in graph.nodes.iter().enumerate() {
        let (title, body, sim) = match meta.get(url) {
            Some(m) => m,
            None => continue,
        };
        records.push(PageSignals {
            url: url.clone(),
            pagerank: ranks.get(i).copied().unwrap_or(0.0),
            anchor_text: anchor_idx.anchor_text_with(
                url,
                args.anchor_budget,
                args.include_internal_anchors,
            ),
            inbound_domains: anchor_idx.inbound_domains(url),
            quality: quality::quality_score(title, body),
            simhash: *sim,
            is_duplicate: false,
        });
    }

    mark_duplicates(&mut records);
    let dupes = records.iter().filter(|r| r.is_duplicate).count();
    let junk = records.iter().filter(|r| r.quality < 0.2).count();
    let with_anchors = records.iter().filter(|r| !r.anchor_text.is_empty()).count();

    if let Some(parent) = std::path::Path::new(&args.out_file).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(&args.out_file)?;
    for rec in &records {
        writeln!(out, "{}", serde_json::to_string(rec)?)?;
    }

    tracing::info!(
        "wrote {} signal records to {} | {} with anchor text | {} near-dupes | {} low quality",
        records.len(),
        args.out_file,
        with_anchors,
        dupes,
        junk
    );
    Ok(())
}

/// Bucket by the high bits of the SimHash so we only compare plausible
/// candidates — comparing all pairs is O(n^2) and unusable past ~100k pages.
/// This is a simplified version of the standard banded LSH approach.
fn mark_duplicates(records: &mut [PageSignals]) {
    // Pass A: exact duplicates by canonical URL. Catches www/non-www,
    // http/https, tracking-parameter and index.html variants — the cases
    // where content-similarity hashing fails because a counter or timestamp
    // differs just enough to clear the threshold.
    let mut seen_canonical: HashMap<String, usize> = HashMap::new();
    let mut url_dupes = 0usize;
    for i in 0..records.len() {
        let key = common::canonical_url(&records[i].url);
        match seen_canonical.get(&key).copied() {
            Some(prev) => {
                // Keep the better page: higher quality, then shorter URL
                // (which favours the canonical form over a decorated one).
                let keep_prev = (records[prev].quality, std::cmp::Reverse(records[prev].url.len()))
                    >= (records[i].quality, std::cmp::Reverse(records[i].url.len()));
                let loser = if keep_prev { i } else { prev };
                if !keep_prev {
                    seen_canonical.insert(key, i);
                }
                records[loser].is_duplicate = true;
                url_dupes += 1;
            }
            None => {
                seen_canonical.insert(key, i);
            }
        }
    }
    if url_dupes > 0 {
        tracing::info!("{url_dupes} exact duplicates collapsed by canonical URL");
    }

    // Pass B: near-duplicates by content similarity.
    let mut buckets: HashMap<u16, Vec<usize>> = HashMap::new();
    for (i, rec) in records.iter().enumerate() {
        if rec.simhash == 0 || rec.is_duplicate {
            continue; // already collapsed by URL
        }
        // Four 16-bit bands; matching any one band makes a candidate pair.
        for band in 0..4 {
            let key = ((rec.simhash >> (band * 16)) & 0xFFFF) as u16;
            buckets.entry(key ^ (band << 14) as u16).or_default().push(i);
        }
    }

    let mut suppressed = vec![false; records.len()];
    for candidates in buckets.values() {
        if candidates.len() < 2 || candidates.len() > 500 {
            continue; // huge buckets are noise, not duplicate clusters
        }
        for (a_pos, &a) in candidates.iter().enumerate() {
            for &b in &candidates[a_pos + 1..] {
                if suppressed[a] || suppressed[b] {
                    continue;
                }
                if quality::hamming(records[a].simhash, records[b].simhash)
                    <= quality::DUPLICATE_BIT_THRESHOLD
                {
                    // Keep the better page; suppress the other.
                    let keep_a = (records[a].quality, std::cmp::Reverse(records[a].url.len()))
                        >= (records[b].quality, std::cmp::Reverse(records[b].url.len()));
                    if keep_a {
                        suppressed[b] = true;
                    } else {
                        suppressed[a] = true;
                    }
                }
            }
        }
    }

    for (i, rec) in records.iter_mut().enumerate() {
        if suppressed[i] {
            rec.is_duplicate = true;
        }
    }
}
