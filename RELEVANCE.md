# How to get Google-quality relevance

The honest framing: Google's relevance advantage is not one algorithm. It's
roughly three things, in descending order of how much they matter and
ascending order of how hard they are to replicate:

1. **Behavioural data.** Billions of clicks telling them which result
   actually answered each query. This is the moat. You cannot buy it or
   derive it — you have to serve real users.
2. **Query understanding.** Mapping what someone typed to what they meant.
3. **Everything else** — crawling, indexing, link analysis, spam filtering.

This repo does (3) properly, has hooks for (2), and (1) is a chicken-and-egg
problem every new search engine faces. What follows is the order I'd actually
build in, with the measured state of each.

---

## Where this engine currently stands

Measured on `eval/judgments.jsonl` (20 queries, 5,512-page Indian-web corpus):

```
full ranking          NDCG@10 0.749   MRR 0.803
text relevance only   NDCG@10 0.760   MRR 0.804
```

**Read that carefully: the auxiliary signals are currently neutral-to-slightly-
negative.** On 20 queries a 0.011 difference is noise, not a result. The
correct conclusion is not "authority doesn't work" — it's *"this judgment set
is too small to tell,"* which is itself the most important finding in the
project. See [Honest limitations](#honest-limitations).

---

## What's built, and what measurement said about it

### 1. Text relevance — BM25F over weighted fields

Separate fields for `title`, `anchor`, `body`, `url_text`, each with its own
boost, plus English stemming so "booking" matches "book".

**Learned the hard way:** the first version blended raw BM25 additively with
authority bonuses. That silently didn't work. Raw BM25 is unbounded and its
range varies per query — on `delhi capital` a keyword-stuffed betting page
scored 81.9 against Wikipedia's Delhi at 50.1, a 31-point gap, while the
entire authority bonus was worth 2.5. Authority could never win.

**Fix:** normalise BM25 to 0..1 within the candidate set *before* blending, so
every weight means what it says. Regression test:
`authority_can_overcome_a_keyword_stuffed_bm25_lead`.

### 2. Anchor text — what other pages call this one

Brin & Page's founding insight: link text describes a target better than the
target's own copy. Implemented in `crates/signals/src/anchors.rs`.

**Measured, on a fixed corpus:**

| Config | NDCG@10 |
|---|---|
| external (cross-domain) anchors, boost 3.0 | **0.753** |
| no anchor text | 0.747 |
| internal + external, boost 0.5 | 0.734 |
| internal + external, boost 3.0 | 0.674 |

Including **internal** (same-site) anchors is clearly harmful here: Wikipedia
links the same handful of phrases at the same handful of pages tens of
thousands of times, which swamps the signal. External-only is the default.

This is worth dwelling on: I originally wrote external-only, "fixed" it to
include internal links because a single-domain corpus produced zero anchors,
and the measurement showed the fix was the bug.

### 3. Link authority — PageRank

Power iteration with dangling-node redistribution and parallel-edge dedup
(`crates/signals/src/pagerank.rs`).

Saturated (`x/(x+k)`) before use, because raw PageRank is wildly skewed and
a handful of hubs would otherwise win every query.

**Hard invariant, enforced by test:**

```
pagerank_weight + domain_trust_weight < text_weight
```

Otherwise a page wins on popularity while barely matching the query. The
unconstrained sweep argmax violated this and measured *worse* anyway.

### 4. Quality and duplicates

Rule-based parked/thin/keyword-stuffed detection, plus 64-bit SimHash
near-duplicate clustering with LSH bucketing. On the current corpus this
removes ~930 near-duplicates and ~100 junk pages out of 5,512.

Deliberately rule-based, not learned: at this stage you need to be able to
read *why* a page was demoted.

---

## The roadmap, in the order I'd build it

### Next: semantic retrieval (hybrid dense + sparse)

The single biggest quality jump available, and the clearest gap between this
and a modern engine. BM25 cannot match "how do I pay someone with my phone"
to a page about UPI — there's no lexical overlap.

- Embed each document (a small sentence-transformer via `fastembed-rs` runs
  locally on CPU; ~100M params is enough).
- Vector index: HNSW (`hnsw_rs` or `usearch`).
- **Fuse, don't average** — use Reciprocal Rank Fusion:
  `RRF(d) = Σ 1/(k + rank_i(d))`, k≈60. RRF combines ranked lists without
  needing the two score scales to be comparable, which is exactly the
  normalisation problem that already bit this codebase once.

Expect the largest single NDCG gain here, especially on tail queries.

### Then: query understanding

Cheap, high-leverage, currently absent:

- **Spelling correction** — an edit-distance-1 lookup against index terms.
- **Synonym / acronym expansion** — "UPI" ↔ "Unified Payments Interface".
- **Intent classification** — navigational vs informational vs transactional.
  These want *different rankings*: navigational should lean on anchor text
  and URL match, informational on body text and authority.
- **Entity recognition** — detect that "Kolkata" is a place and prefer the
  canonical page over pages that merely mention it. This directly fixes an
  observed failure where oxygen-rental pages beat the Kolkata article.

### Then: learning to rank

Once you have ≥200 judged queries, replace the hand-weighted linear blend
with a trained model (LambdaMART via `lightgbm`, or a small cross-encoder to
rerank the top 50). Hand-tuned weights plateau fast; the eval harness is
already the training-data pipeline.

### Then: behavioural signals

The real moat, and the reason this list ends here rather than continuing.
Once you have users: click-through rate per query-document pair, dwell time,
and pogo-sticking (returning to results = bad result). This dwarfs every
signal above — and it's why a new entrant's honest pitch is a *vertical*
where they have data or judgment Google lacks, not "better general web
search".

---

## Honest limitations

Things a founder will spot in five minutes, so say them first:

- **The judgment set is too small.** 20 queries cannot resolve differences
  below ~0.05 NDCG. Every signal comparison above is directional at best.
  Fix: 200+ queries, and a paired significance test.
- **Judgments are mine, and partly pooled from this engine's own output** —
  which biases toward what it already returns. Proper practice is to pool
  candidates across several systems and judge blind.
- **Corpus is small and skewed** (~5.5k pages, Wikipedia-heavy plus a
  Common Crawl `.in` slice). Link-graph signals are weak at this scale
  because most links point outside the corpus.
- **No freshness signal.** Common Crawl snapshots are monthly.
- **Quality rules are hand-written** and will not generalise to adversarial
  spam.
- **Dedup misses `www.` vs non-`www.` variants** whose bodies differ slightly
  — URL canonicalisation should run before SimHash.

---

## Running the measurements

```bash
cargo run --release -p signals                     # compute offline signals
cargo run --release -p indexer --bin build-index   # build index
cargo run --release -p api &                       # serve

cargo run --release -p eval                        # NDCG@10 / MRR / P@10
cargo run --release -p eval -- --verbose           # see ranked lists
./eval/ablation.sh                                 # per-signal contribution
./eval/sweep.sh                                    # grid-search weights
```

**The rule:** change one thing, re-run `eval`, keep it only if the mean
improves. If it improved one query and dropped the mean, you overfit.
