mod robots;

use anyhow::Result;
use clap::Parser;
use common::{shard_for_url, CrawledPage, OutLink};
use robots::RobotsRules;
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const USER_AGENT: &str = "rust-search-engine-bot/0.1 (+contact: apaarmeet5000@gmail.com)";

/// A polite, shardable BFS web crawler.
///
/// Scaling model: the URL space is split into `num_shards` buckets by hash
/// (see `common::shard_for_url`). This worker only fetches URLs in its own
/// `shard_id`. Links discovered for *other* shards are written to a handoff
/// file instead of being fetched — in a real deployment that handoff file is
/// a message queue (Kafka/SQS/etc.) and a sibling worker process owns each
/// other shard. Run `num_shards` of these processes (one per shard id,
/// potentially one per machine) and you have a horizontally scaled crawler
/// with no code changes, only more processes.
#[derive(Parser, Debug)]
struct Args {
    /// File with one seed URL per line.
    #[arg(long)]
    seeds: String,
    /// Directory to write crawled pages (JSONL, one file per shard) and
    /// cross-shard handoff files.
    #[arg(long, default_value = "data/crawled")]
    out_dir: String,
    /// Stop after fetching this many pages (successful fetches only).
    #[arg(long, default_value_t = 200)]
    max_pages: usize,
    /// Total number of shards in the deployment.
    #[arg(long, default_value_t = 1)]
    num_shards: u32,
    /// Which shard this worker is responsible for (0..num_shards).
    #[arg(long, default_value_t = 0)]
    shard_id: u32,
    /// Max concurrent in-flight requests.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
}

struct FetchResult {
    url: String,
    page: Option<CrawledPage>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    std::fs::create_dir_all(&args.out_dir)?;
    let seeds: Vec<String> = std::fs::read_to_string(&args.seeds)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()?;

    let mut frontier: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut robots_cache: HashMap<String, RobotsRules> = HashMap::new();
    let mut last_fetch: HashMap<String, Instant> = HashMap::new();

    for seed in seeds {
        if shard_for_url(&seed, args.num_shards) == args.shard_id {
            frontier.push_back(seed.clone());
            visited.insert(seed);
        }
    }

    let out_path = format!("{}/shard-{}.jsonl", args.out_dir, args.shard_id);
    let mut out_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)?;

    let mut fetched = 0usize;

    while fetched < args.max_pages && !frontier.is_empty() {
        let batch_size = args.concurrency.min(frontier.len());
        let batch: Vec<String> = (0..batch_size).filter_map(|_| frontier.pop_front()).collect();

        let mut join_set = tokio::task::JoinSet::new();
        for url in batch {
            // Politeness: per-domain robots.txt check + minimum delay.
            let domain = match Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)) {
                Some(d) => d,
                None => continue,
            };

            if !robots_cache.contains_key(&domain) {
                let rules = fetch_robots(&client, &url).await;
                robots_cache.insert(domain.clone(), rules);
            }
            let rules = robots_cache.get(&domain).unwrap();
            let path = Url::parse(&url).map(|u| u.path().to_string()).unwrap_or_default();
            if !rules.is_allowed(&path) {
                tracing::info!("robots.txt disallows {url}");
                continue;
            }

            if let Some(last) = last_fetch.get(&domain) {
                let elapsed = last.elapsed();
                let min_delay = Duration::from_millis(rules.crawl_delay_ms);
                if elapsed < min_delay {
                    tokio::time::sleep(min_delay - elapsed).await;
                }
            }
            last_fetch.insert(domain.clone(), Instant::now());

            let client = client.clone();
            join_set.spawn(async move { fetch_one(&client, url).await });
        }

        while let Some(joined) = join_set.join_next().await {
            let result = joined?;
            if let Some(page) = &result.page {
                let line = serde_json::to_string(page)?;
                writeln!(out_file, "{line}")?;
                fetched += 1;

                for link in page.outlink_urls() {
                    if visited.contains(link) {
                        continue;
                    }
                    visited.insert(link.to_string());
                    let target_shard = shard_for_url(link, args.num_shards);
                    if target_shard == args.shard_id {
                        frontier.push_back(link.to_string());
                    } else {
                        append_handoff(&args.out_dir, target_shard, link)?;
                    }
                }
                tracing::info!(fetched, queued = frontier.len(), "crawled {}", result.url);
            }
            if fetched >= args.max_pages {
                break;
            }
        }
    }

    tracing::info!("done: fetched {fetched} pages into {out_path}");
    Ok(())
}

async fn fetch_robots(client: &reqwest::Client, any_url_on_domain: &str) -> RobotsRules {
    let Ok(mut url) = Url::parse(any_url_on_domain) else {
        return RobotsRules::default();
    };
    url.set_path("/robots.txt");
    url.set_query(None);
    match client.get(url.as_str()).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            RobotsRules::parse(&body, "rust-search-engine-bot")
        }
        _ => RobotsRules::default(),
    }
}

async fn fetch_one(client: &reqwest::Client, url: String) -> FetchResult {
    let page = match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                None
            } else {
                match resp.text().await {
                    Ok(body) => Some(parse_page(&url, status, &body)),
                    Err(_) => None,
                }
            }
        }
        Err(e) => {
            tracing::warn!("fetch failed for {url}: {e}");
            None
        }
    };
    FetchResult { url, page }
}

fn parse_page(url: &str, status: u16, body: &str) -> CrawledPage {
    let doc = Html::parse_document(body);
    let title_sel = Selector::parse("title").unwrap();
    let title = doc
        .select(&title_sel)
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string();

    // NOTE: `ElementRef::text()` walks all descendant text nodes, which in
    // HTML includes raw <script>/<style> contents (those are stored as text
    // nodes per the HTML spec). That pollutes relevance for JS-heavy pages.
    // Good enough for this scaffold; a real indexer should either strip
    // script/style subtrees via a DOM walk first, or run extraction through
    // a proper readability/boilerplate-removal pass.
    let body_sel = Selector::parse("body").unwrap();
    let body_text: String = doc
        .select(&body_sel)
        .next()
        .map(|el| el.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let body_text: String = body_text.split_whitespace().collect::<Vec<_>>().join(" ");

    let a_sel = Selector::parse("a[href]").unwrap();
    let base = Url::parse(url).ok();
    let mut links: Vec<OutLink> = Vec::new();
    for el in doc.select(&a_sel) {
        if let Some(href) = el.value().attr("href") {
            let resolved = base.as_ref().and_then(|b| b.join(href).ok());
            if let Some(mut u) = resolved {
                u.set_fragment(None);
                if u.scheme() == "http" || u.scheme() == "https" {
                    // Anchor text is captured here, not discarded: it is the
                    // strongest non-ML relevance signal available.
                    let text = el.text().collect::<String>();
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    links.push(OutLink { url: u.to_string(), text });
                }
            }
        }
    }
    links.sort_by(|a, b| a.url.cmp(&b.url).then(a.text.cmp(&b.text)));
    links.dedup_by(|a, b| a.url == b.url && a.text == b.text);

    CrawledPage {
        url: url.to_string(),
        title,
        body_text,
        links,
        crawled_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        status,
    }
}

fn append_handoff(out_dir: &str, target_shard: u32, url: &str) -> Result<()> {
    let path = format!("{out_dir}/handoff-shard-{target_shard}.txt");
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{url}")?;
    Ok(())
}
