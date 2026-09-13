# Relevance evaluation

This directory is the difference between tuning and guessing.

## The judgment set

`judgments.jsonl` — one line per query:

```json
{"query": "indian railways booking", "judgments": {"https://...": 3, "https://...": 1}}
```

Grades follow the TREC convention:

| Grade | Meaning |
|---|---|
| **3** | This *is* the answer. The user is done. |
| **2** | Very useful, clearly on-topic. |
| **1** | Marginally related; user might click it. |
| **0** | Irrelevant. (Omit — unjudged defaults to 0.) |

Graded rather than binary judgments are what make NDCG meaningful: getting
a 3 at rank 1 should beat getting a 1 at rank 1, and binary labels can't
express that.

## Workflow

```bash
cargo run --release -p api &            # serve the index
cargo run --release -p eval             # score it
cargo run --release -p eval -- --verbose  # see the actual ranked lists
```

Then: change one weight in `crates/api/src/rank.rs`, rebuild, re-run.
**If mean NDCG@10 drops, revert** — regardless of how much better the change
made your favourite query look. Single-query tuning is how ranking systems
quietly rot.

## Building a judgment set honestly

The trap is judging results the engine already returned — that bakes your
current ranking into the ground truth and every future change looks worse.

Instead:
1. Write the queries **first**, from what users would actually type, before
   looking at any output.
2. **Pool** candidates: run several different configurations (BM25-only,
   with-anchors, high-authority) and judge the union of the top 10 from each.
   This is the standard TREC pooling method and it's what keeps the judgments
   from being biased toward one system.
3. Judge the URL against the *query intent*, not against where it ranked.

## Query mix

A judgment set that's all one query type will mislead you. Aim for a spread:

- **Navigational** — "irctc", "sbi net banking". One right answer; MRR is the
  metric that matters. Anchor text and URL matching dominate.
- **Informational** — "how does upi work", "monsoon season india". Many
  acceptable answers; NDCG is the metric. Body text dominates.
- **Transactional** — "cheap flights delhi mumbai". Freshness and quality
  matter more than authority.
- **Head vs tail** — a few very common queries, plus rare specific ones.
  Tail queries are where recall problems hide.

## Known limitation

Small judgment sets are noisy. A 0.02 NDCG move on 20 queries is not a
result — it's variance. Either grow the set to 50+ queries, or use a
paired significance test before believing a small delta.
