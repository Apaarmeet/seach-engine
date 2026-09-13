//! Fetching and parsing individual WARC records.
//!
//! WARC files in Common Crawl are *per-record* gzipped, which is what makes
//! this whole approach work: a byte range taken from the middle of a 1GB
//! WARC is itself a valid gzip stream. So we fetch ~30KB instead of 1GB.
//!
//! A decompressed record looks like:
//!   WARC/1.0 headers  \r\n\r\n  HTTP/1.1 headers  \r\n\r\n  <html>...

use anyhow::{anyhow, Result};
use common::{CrawledPage, OutLink};
use flate2::read::MultiGzDecoder;
use scraper::{Html, Selector};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const CC_BASE: &str = "https://data.commoncrawl.org";

pub fn gunzip_to_string(bytes: &[u8]) -> Result<String> {
    let mut decoder = MultiGzDecoder::new(bytes);
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf)?;
    // WARC payloads are arbitrary bytes; lossy decoding keeps us moving
    // rather than dropping a page over one bad byte sequence.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Byte-range fetch a single WARC record and extract a `CrawledPage`.
pub async fn fetch_and_parse(
    client: &reqwest::Client,
    warc_filename: &str,
    offset: u64,
    length: u64,
    url: &str,
) -> Result<CrawledPage> {
    let target = format!("{CC_BASE}/{warc_filename}");
    let end = offset + length - 1;
    let bytes = client
        .get(&target)
        .header("Range", format!("bytes={offset}-{end}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let record = gunzip_to_string(&bytes)?;
    parse_record(&record, url)
}

fn parse_record(record: &str, url: &str) -> Result<CrawledPage> {
    // Skip the WARC header block, then the HTTP header block, to reach HTML.
    let after_warc = record
        .split_once("\r\n\r\n")
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("no WARC header terminator"))?;
    let (http_headers, html) = after_warc
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("no HTTP header terminator"))?;

    let status = http_headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(200);

    Ok(extract(url, status, html))
}

/// Turn raw HTML into the same `CrawledPage` shape the live crawler emits,
/// so the existing indexer and ranker consume Common Crawl data unchanged.
pub fn extract(url: &str, status: u16, html: &str) -> CrawledPage {
    let doc = Html::parse_document(html);

    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| doc.select(&sel).next().map(|e| e.text().collect::<String>()))
        .unwrap_or_default()
        .trim()
        .to_string();

    // Unlike the live crawler, strip <script>/<style> subtrees properly:
    // collect their text first, then subtract. Cheap, and it keeps JS blobs
    // out of the relevance signal.
    let body_text = extract_visible_text(&doc);

    let base = Url::parse(url).ok();
    let mut links: Vec<OutLink> = Vec::new();
    if let (Some(base), Ok(a_sel)) = (base.as_ref(), Selector::parse("a[href]")) {
        for el in doc.select(&a_sel) {
            if let Some(href) = el.value().attr("href") {
                if let Ok(mut u) = base.join(href) {
                    u.set_fragment(None);
                    if u.scheme() == "http" || u.scheme() == "https" {
                        let text = el.text().collect::<String>();
                        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                        links.push(OutLink { url: u.to_string(), text });
                    }
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

/// Text of the document with script/style/noscript content removed.
fn extract_visible_text(doc: &Html) -> String {
    let Ok(body_sel) = Selector::parse("body") else {
        return String::new();
    };
    let Some(body) = doc.select(&body_sel).next() else {
        return String::new();
    };

    // Ids of nodes inside script/style/noscript, so their text nodes can be
    // skipped while walking the tree.
    let mut skip = std::collections::HashSet::new();
    if let Ok(sel) = Selector::parse("script, style, noscript, template") {
        for el in doc.select(&sel) {
            for descendant in el.descendants() {
                skip.insert(descendant.id());
            }
        }
    }

    let mut out = String::new();
    for node in body.descendants() {
        if skip.contains(&node.id()) {
            continue;
        }
        if let Some(text) = node.value().as_text() {
            let t = text.trim();
            if !t.is_empty() {
                out.push_str(t);
                out.push(' ');
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_and_style_from_body_text() {
        let html = r#"<html><body>
            <h1>Chennai News</h1>
            <script>var tracking = "should_not_be_indexed";</script>
            <style>.x { color: red; }</style>
            <p>Monsoon update</p>
        </body></html>"#;
        let page = extract("https://example.in/", 200, html);
        assert!(page.body_text.contains("Chennai News"));
        assert!(page.body_text.contains("Monsoon update"));
        assert!(!page.body_text.contains("should_not_be_indexed"));
        assert!(!page.body_text.contains("color: red"));
    }

    #[test]
    fn parses_warc_record_into_page() {
        let record = "WARC/1.0\r\nWARC-Type: response\r\n\r\n\
                      HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                      <html><head><title>Hi</title></head><body>\
                      <a href=\"/next\">n</a>body text</body></html>";
        let page = parse_record(record, "https://example.in/page").unwrap();
        assert_eq!(page.title, "Hi");
        assert_eq!(page.status, 200);
        assert!(page.body_text.contains("body text"));
        assert_eq!(page.links[0].url, "https://example.in/next");
        assert_eq!(page.links[0].text, "n");
    }
}
