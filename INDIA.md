# Indexing the Indian web

Goal: index "all websites of India" without crawling the web from scratch.

## The shortcut that makes it feasible

Common Crawl's CDX index is sorted by **SURT key**, which reverses the host:

```
www.flipkart.in/deals  ->  in,flipkart,www)/deals
```

Because the host is reversed, **every domain under a TLD is contiguous** in
the sorted index. Selecting all of `.in` is therefore a *range scan*, not a
full scan — you read the ~100MB sparse `cluster.idx`, keep the blocks whose
key starts with `in,`, and byte-range fetch only those.

Common Crawl gzips each WARC record independently, so a byte range pulled
from the middle of a 1GB WARC file is itself a valid gzip stream. Net effect:
you download ~30KB per page instead of a 100TB crawl.

No AWS account needed — everything goes over `https://data.commoncrawl.org`.

## Measured numbers (CC-MAIN-2026-34, August 2026)

Run `cc-ingest estimate` to reproduce these:

| | |
|---|---|
| Whole crawl | 873,102 blocks ≈ **2.62B pages** (~31 TB of HTML) |
| `.in` blocks | 5,765 = **0.66%** of the crawl |
| **`.in` pages** | **17.3M** captures, **12.9M** are HTTP 200 + HTML |
| Avg WARC record | 30.8 KB |
| **Download** | **~0.41 TB** |
| CDX index for `.in` | 1.64 GB |

Derived: ~90 GB of extracted text, ~50 GB tantivy index. **Fits on one NVMe.**
At 200 Mbps the download is ~5 hours; at 1 Gbps, ~1 hour.

## Usage

```bash
# measure before committing to a download
cargo run --release -p cc-ingest -- estimate

# pull the whole .in ccTLD (~0.41 TB, resumable by re-running)
cargo run --release -p cc-ingest -- fetch --concurrency 32

# then the normal pipeline
cargo run --release -p ranker
cargo run --release -p indexer --bin build-index -- \
    --pagerank-file ranker-data/pagerank.jsonl
cargo run --release -p api
```

Start small: `--max-blocks 50 --max-pages 5000` gives a working index in
minutes before you commit to the full run.

## ".in" is not the same as "Indian"

This is the honest gap, and it cuts both ways.

**Missing:** many of India's biggest sites are on `.com` — `flipkart.com`,
`indiatimes.com`, `zomato.com`, `hdfcbank.com`. The `in,` prefix does not
touch them. Add them explicitly (SURT form, repeat the flag):

```bash
cc-ingest fetch --prefix 'in,' \
                --prefix 'com,flipkart)' \
                --prefix 'com,indiatimes)'
```

For broad coverage beyond a curated list you need a selector other than TLD:

- **Language.** Catch Hindi/Tamil/Bengali/etc. content on any TLD. CDX is not
  sorted by language, so this needs Common Crawl's **columnar (Parquet)
  index**, which has a `content_languages` field — but that path requires AWS
  credentials to list/glob S3.
- **IP geolocation** of the host. Imperfect; CDNs break it.

**Included but unwanted:** a chunk of `.in` registrations are parked or
for-sale placeholder pages. In the alphabetically-first block (numeric
domains like `001bz.in`) these dominated; deeper into the range a sample of
448 pages was 98% real content, median ~6,900 chars. Either way you want a
parked-domain filter before trusting relevance — match on titles like
"Domain For Sale" / "Please access via the domain name" and drop
low-text-volume pages.

## What this still won't be

Coverage is one axis; relevance is another. Even with all 17M pages indexed,
ranking here is BM25 + PageRank — roughly 2001-era. Missing: near-duplicate
detection, spam/quality classifiers, query understanding (spelling,
synonyms, intent), personalization, and freshness. Common Crawl snapshots are
monthly, so the index is weeks stale by construction; pair it with the live
`crawler` for domains that need freshness.
