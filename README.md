# rsearch — a search engine from scratch (Rust + React)

A working web search engine: polite crawler → inverted index → offline
link-graph ranking → query API → React UI. Every stage is sharded by URL
hash, so scaling out means running more shard processes, not rewriting code.

## Reality check on "like Google"

The *architecture* here is the same shape Google uses. The *scale* is not,
and that gap is capital, not code:

| | Google | this, on one laptop |
|---|---|---|
| Pages indexed | ~100B+ | whatever you crawl |
| Machines | data centers | 1 |
| Ranking signals | hundreds | BM25 + PageRank |

What makes this a real search engine rather than a toy is that nothing in
the design *prevents* scale — see [Scaling path](#scaling-path). What you
can't do solo is pay for the crawl bandwidth, storage, and fleet needed to
cover the open web. The standard way around that is to start from
[Common Crawl](https://commoncrawl.org), a free petabyte-scale public crawl,
instead of discovering the web yourself.

## Architecture

```
seeds.txt
   │
   ▼
┌─────────┐  JSONL pages   ┌──────────┐  link graph   ┌────────┐
│ crawler │───────────────▶│  ranker  │──────────────▶│ scores │
└─────────┘                └──────────┘               └────┬───┘
   │ robots.txt aware                                      │
   │ per-domain rate limit                                 │
   ▼                                                       ▼
data/crawled/shard-N.jsonl ────────────▶ ┌─────────┐ ◀─────┘
                                          │ indexer │
                                          └────┬────┘
                                               │ tantivy index
                                               ▼
                                          ┌────────┐   HTTP    ┌───────┐
                                          │  api   │◀─────────▶│ React │
                                          └────────┘  /search  └───────┘
```

| Crate | Role |
|---|---|
| `common` | Shared types + `shard_for_url()` — the one hash function every stage agrees on |
| `crawler` | Async BFS crawler. Respects robots.txt, rate-limits per domain, writes JSONL. Links for other shards go to a handoff file |
| `ranker` | Offline PageRank power-iteration over the crawled link graph (handles dangling nodes) |
| `indexer` | Builds a [tantivy](https://github.com/quickwit-oss/tantivy) inverted index; folds in PageRank as a fast field |
| `signals` | Offline pass: inbound anchor text, PageRank, quality scoring, SimHash dedup |
| `api` | Axum HTTP server. BM25F + blended signals, parallel shard fan-out, `?explain=true` |
| `eval` | NDCG@10 / MRR / P@10 against a judgment set — see [RELEVANCE.md](RELEVANCE.md) |
| `places` | Local "near me" search over OpenStreetMap POIs — see [LOCAL-SEARCH.md](LOCAL-SEARCH.md) |
| `cc-ingest` | Pulls a slice of Common Crawl (e.g. the whole `.in` ccTLD) into the same JSONL the crawler emits — see [INDIA.md](INDIA.md) |

**Ranking** = normalised BM25F over title/anchor/body/url, blended with
saturated PageRank, quality and domain-trust signals. Every weight is tuned
against a judgment set rather than by eye — see **[RELEVANCE.md](RELEVANCE.md)**
for the measured contribution of each signal, including the ones that turned
out to hurt.

## Quickstart

```bash
# 1. crawl (edit seeds.txt first)
cargo run --release -p crawler -- --seeds seeds.txt --max-pages 200

# 2. compute offline relevance signals (anchors, authority, quality, dedup)
cargo run --release -p signals

# 3. build the index
cargo run --release -p indexer --bin build-index

# 4. serve
cargo run --release -p api          # :8080

# 5. UI (separate terminal)
cd frontend && npm install && npm run dev   # :5173
```

Re-run steps 1–3 to refresh the index; the API picks up commits without a
restart.

## Scaling path

Each step is additive — no rewrite between them.

1. **One machine, one shard** (today). Good to ~millions of pages.
2. **One machine, N shards.** Run N crawlers with `--num-shards N
   --shard-id i`, build N indexes, start the API with
   `--index-dirs index0,index1,...`. Query fan-out is already parallel.
3. **N machines.** Same commands, different hosts. Replace the
   `handoff-shard-N.txt` files with a real queue (Kafka/SQS) and the local
   index dirs with per-host API instances behind a coordinator.
4. **Skip the crawl.** Point the indexer at Common Crawl WARC files instead
   of crawling the web yourself. This is the only realistic route to
   billions of pages.

## Known limitations

These are real and worth fixing before trusting relevance:

- **Text extraction in the live `crawler` is crude.** `ElementRef::text()`
  includes `<script>`/`<style>` contents, polluting BM25 on JS-heavy pages.
  `cc-ingest` already does this correctly (`warc::extract_visible_text`,
  unit-tested) — port that function into `crawler`.
- **robots.txt parsing is minimal** — no wildcards, no `Allow:` precedence.
  Swap in `texting_robots` for production.
- **The frontend renders API snippets via `dangerouslySetInnerHTML`.** Safe
  only because tantivy HTML-escapes page text before inserting its own `<b>`
  tags. Re-verify if you change snippet generation.
- **No deduplication, no spam/quality signals, no query understanding**
  (spelling, synonyms, intent). These are where most of Google's real
  relevance advantage lives.
- **In-memory crawl frontier.** Fine for thousands of pages; needs a
  disk/queue-backed frontier beyond that.

## Verified

Crawled 10 real pages (rust-lang.org, Wikipedia), indexed them, and queried:

```
"pagerank algorithm"    → PageRank — Wikipedia (63.5), Search engine — Wikipedia (57.4)
"rust memory safety"    → Rust Programming Language (23.6)
"web crawler indexing"  → Search engine — Wikipedia (59.2)
```

Sub-5ms queries end to end through the React dev proxy.
