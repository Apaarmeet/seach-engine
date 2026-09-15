# Finding the exact URL

> "We should be able to find any url sub url on the web, even obscure ones —
> even a local radio station in Moscow's website." — including spelling
> mistakes.

That is a different problem from the one the web index next door solves, and
it needs a different machine. This document is what got built, why each
piece exists, and what it measurably does and does not do.

## The problem is recall, not ranking

Two queries defined the task:

| query | wanted |
|---|---|
| `valuepickr bajaj finance` | `forum.valuepickr.com/t/bajaj-finance-limited/267` |
| `saint edmunds school shillong` | `stedmundshillong.in` |

Neither target is in this project's crawl — 5,512 pages, most of them parked
`.in` domains. Recall on both was **zero**, so no amount of BM25F or PageRank
tuning could have moved them. The corpus was the problem, and "crawl more"
does not fix the long tail: the long tail is, by definition, the part you did
not crawl.

So this resolves rather than retrieves.

## The pipeline

```
  "saint edmunds school shillong"
     │
  1. read it        variants (saint|st, edmunds|edmund), site/page splits
     │
  2. ask            OSM website tags · Wikidata P856 · a web index (optional)
     │
  3. guess          stedmundshillong.in, stedmunds.in, … (hundreds)
     │
  4. check cheaply  DNS: which of those exist at all?          (a handful)
     │
  5. find the page  the site's own sitemap, matched on slug
     │
  6. confirm        fetch it — does the page say what was asked for?
     ▼
  stedmundshillong.in  "St. Edmund's School, Shillong"   confidence 1.00
```

Steps 3 and 4 are the load-bearing pair. Guessing is only affordable because
DNS answers *"does this name exist?"* for a UDP round trip — no page fetch,
no crawl budget, nobody's robots.txt involved. A typical query invents ~220
hostnames, ~15 of them exist, and ~6 are worth retrieving.

Step 6 is what separates this from a search-API proxy. Every answer was
fetched and checked before it was shown.

## Four sources, failing in different directions

| source | reaches | misses |
|---|---|---|
| **Hostname synthesis** | anything whose domain is derivable from its name — `stedmundshillong.in` | opaque abbreviations |
| **OSM places index** | POIs someone tagged with a website — `puchd.ac.in` | untagged entities; not pages |
| **Wikidata** (P856) | notable organisations — `cuchd.in`, `echo.msk.ru` | non-notable entities; not pages |
| **Web index** (optional) | deep pages — `cuchd.in/computing/bachelor-of-computer-applications.php` | needs an API key |

Running all four is worth it precisely because they fail differently. OSM and
Wikidata know Chandigarh University is `cuchd.in`, which no spelling rule
could produce. Neither knows St. Edmund's School's website, which guessing
finds on the first try.

The web index is configured by environment variable and is entirely
optional — `SERPER_API_KEY`, `GOOGLE_CSE_KEY`+`GOOGLE_CSE_CX`, `SEARXNG_URL`,
or `BRAVE_API_KEY`. With none set, everything else still runs.

### Why a search API is a source and not the answer

Handing back a search engine's first result is not this system. Results from
any source enter as *candidates*: ranked against the query, fetched,
quality-checked, and dropped if they cannot be confirmed. "Google's top
result" and "a URL we retrieved and verified answers your query" are
different claims, and only the second one reaches the user.

## The failure that shapes everything: parked domains

Guessed hostnames are, by construction, exactly the names squatters
register. A parked page is worse than no answer, because the user believes
it. Three defences, all necessary:

1. **Phrase matching** — `signals::quality`, shared with the indexer.
2. **Structural** — a page titled only after its own domain is selling it.
3. **Place agreement** — `saintedmunds.org` is a real St. Edmund's in
   California. Perfect name match, wrong continent. A named place is a
   constraint, not a hint.

`govorit-moskva.ru` scored a **perfect 1.00 on relevance** — it was built
from the query's own words — and was rejected on quality alone.

## Measured

`eval/navigational.jsonl`, 24 hand-verified cases. Every gold URL was fetched
and confirmed live, independently of the resolver. Entities were chosen
first and their URLs looked up second, so the set is not a list of things
that already worked — several cases are there because they were expected to
fail.

```
cargo run --release -p eval --bin nav -- --api http://localhost:8080
```

The metric is **success@1**: the user wanted one page, and the ninth-best
result is not partial credit. Failures are split, because they are not
equally bad — a blank result makes the user search again, a confident wrong
URL makes them click it and conclude the product is broken.

| | |
|---|---|
| success@1 | 62% |
| wrong answer (costly) | **0%** |
| no answer (safe) | 38% |
| site+page queries | 7/7 |
| abbreviations (`nit rourkela` → `nitrkl.ac.in`) | 0/3 |

Getting wrong answers to zero mattered more than the headline number. Two
earlier regressions — a construction firm returned for a school, a squatted
domain returned for a radio station — are now covered by tests.

## Known limits

- **Abbreviations that are not initials.** `iitkgp`, `nitrkl` are consonant
  skeletons, not acronyms. Initials (`jnu`) are generated; these are not.
- **Slug matching cannot expand acronyms.** Chandigarh University's BCA page
  is `bachelor-of-computer-applications.php` — the slug contains no "bca", so
  sitemap matching alone cannot find it. A web index can, because it knows
  the expansion.
- **Sites that block bots.** `iimb.ac.in` returns 403 to an honest crawler
  user-agent. Unresolvable without pretending to be a browser.
- **Latency is ~8–15s.** DNS is fast (~300ms warm); the time is live fetches
  against third-party servers. The UI therefore paints index results
  immediately and fills the resolved URL in when it arrives, rather than
  blocking on it.
- **The gazetteer's place gate is coarse.** OSM contains a village called
  "Saint" and GeoNames a town called "Same"; both required guards, and the
  class of collision is open-ended.
