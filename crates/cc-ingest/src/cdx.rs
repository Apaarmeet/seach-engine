//! Locating pages in Common Crawl without downloading the whole crawl.
//!
//! The trick that makes this cheap: CDX index lines are sorted by SURT key,
//! which reverses the host (`example.in` -> `in,example)/`). That makes every
//! domain under a TLD **contiguous** in the index, so selecting a whole
//! ccTLD is a range scan rather than a full scan.
//!
//! `cluster.idx` is the sparse top-level index (~100MB): one line per ~3000
//! URL block, giving the block's `cdx-*.gz` file, byte offset and length.
//! We keep only the blocks whose SURT starts with our prefix, then
//! byte-range fetch just those blocks.

use anyhow::{Context, Result};
use serde::Deserialize;

const CC_BASE: &str = "https://data.commoncrawl.org";

/// One entry from cluster.idx: a byte range inside a cdx-NNNNN.gz file.
#[derive(Debug, Clone)]
pub struct BlockRef {
    pub surt: String,
    pub file: String,
    pub offset: u64,
    pub length: u64,
}

/// One capture record from a CDX block — a single crawled URL and where its
/// content lives inside a WARC file.
#[derive(Debug, Clone, Deserialize)]
pub struct CdxRecord {
    pub url: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub languages: Option<String>,
    pub filename: String,
    /// CDX encodes these as JSON strings, not numbers.
    pub offset: String,
    pub length: String,
}

impl CdxRecord {
    pub fn warc_range(&self) -> Option<(u64, u64)> {
        let off = self.offset.parse().ok()?;
        let len: u64 = self.length.parse().ok()?;
        Some((off, len))
    }

    pub fn is_fetchable_html(&self) -> bool {
        self.status == "200"
            && (self.mime.contains("html") || self.mime.is_empty())
    }
}

pub fn cluster_idx_url(crawl: &str) -> String {
    format!("{CC_BASE}/cc-index/collections/{crawl}/indexes/cluster.idx")
}

/// Parse cluster.idx, keeping only blocks whose SURT key falls under one of
/// `prefixes` (e.g. `["in,"]` for the whole .in ccTLD).
///
/// Note this is prefix matching on the *block's first key*. A block whose
/// first key sorts just before the prefix can still contain matching URLs,
/// so we also keep the one block immediately preceding each matched run —
/// otherwise we'd silently drop the first few thousand URLs of the range.
pub fn select_blocks(cluster_idx: &str, prefixes: &[String]) -> Vec<BlockRef> {
    let lines: Vec<&str> = cluster_idx.lines().collect();
    let matches = |s: &str| prefixes.iter().any(|p| s.starts_with(p.as_str()));

    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        let surt = line.split('\t').next().unwrap_or("");
        if matches(surt) {
            keep[i] = true;
            // Include the preceding block: it may straddle the boundary.
            if i > 0 {
                keep[i - 1] = true;
            }
        }
    }

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        let mut f = line.split('\t');
        let surt = f.next().unwrap_or("").to_string();
        let file = f.next().unwrap_or("").to_string();
        let offset = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let length = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        if file.is_empty() || length == 0 {
            continue;
        }
        out.push(BlockRef { surt, file, offset, length });
    }
    out
}

/// Byte-range fetch one CDX block and parse its records.
pub async fn fetch_block(
    client: &reqwest::Client,
    crawl: &str,
    block: &BlockRef,
    prefixes: &[String],
) -> Result<Vec<CdxRecord>> {
    let url = format!("{CC_BASE}/cc-index/collections/{crawl}/indexes/{}", block.file);
    let end = block.offset + block.length - 1;
    let body = client
        .get(&url)
        .header("Range", format!("bytes={}-{}", block.offset, end))
        .send()
        .await?
        .error_for_status()
        .context("cdx block fetch")?
        .bytes()
        .await?;

    let text = crate::warc::gunzip_to_string(&body)?;
    let mut out = Vec::new();
    for line in text.lines() {
        // Format: <surt> <timestamp> <json>
        let mut parts = line.splitn(3, ' ');
        let surt = parts.next().unwrap_or("");
        let _ts = parts.next();
        let Some(json) = parts.next() else { continue };

        // Blocks can straddle the prefix boundary, so re-check per line.
        if !prefixes.iter().any(|p| surt.starts_with(p.as_str())) {
            continue;
        }
        match serde_json::from_str::<CdxRecord>(json) {
            Ok(rec) => out.push(rec),
            Err(_) => continue, // malformed/redirect-only rows
        }
    }
    Ok(out)
}
