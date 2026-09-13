# Local search — "places near me"

## Why this is a separate index

"Cafés near me" is not a harder web search. It's a different query against a
different corpus:

| | Web search | Local search |
|---|---|---|
| Corpus | crawled pages | points of interest |
| Retrieval | inverted index on text | geospatial + text |
| Primary signal | text relevance | **distance** |
| Authority from | inbound links | completeness / popularity |

Web pages have no coordinates and POIs have no inbound links, so one ranking
function cannot serve both. `places-index` is therefore a parallel index with
its own schema, its own scoring, and its own endpoint (`/nearby`).

## The three things you need

### 1. A places corpus

**OpenStreetMap**, fetched **lazily per tile** — not downloaded up front,
and not queried live per request. Both of those are wrong, measured:

| Approach | Latency | Notes |
|---|---|---|
| Overpass live, per query | **1,170–7,690 ms** | 1 of 3 sample runs failed outright |
| Local index | **0 ms** | but needs data to already be there |

OSM's [usage policy](https://operations.osmfoundation.org/policies/api/) is
explicit: *"Maximum of 2 download threads"*, *"Heavy Users: we request that
you set up your own data server"*, *"Clients may be blocked without notice"*.
Overpass is a community resource, not a backend.

But downloading the whole 1.71 GB country extract is also wrong — it pays
for all of India to answer a query about one neighbourhood, and it is stale
the moment it lands.

**So: lazy tile caching.** Divide the world into ~5 km geohash tiles. Fetch
a tile the first time someone searches inside it, index it, serve every
later query there locally. Measured, starting from an empty index:

```
COLD  Koramangala, never fetched   1 tile, 3,366 places   28,056 ms
WARM  same area, same query        0 tiles                     0 ms
WARM  "pharmacy" (same tile)       0 tiles                     0 ms
WARM  "atm"       (same tile)      0 tiles                     0 ms
```

One slow request per ~25 km², then everything in that area is instant —
including queries for *different* categories, because the tile holds all
POI types. Cost scales with where your users actually are.

Tunables: `--tile-ttl` (default 7 days) controls refresh;
`--no-lazy-fetch` disables outbound calls entirely and serves only what is
already indexed.

#### When lazy fetching stops being the right answer

Lazy tiles are right for on-demand use in a few areas. They are wrong the
moment you want broad coverage up front, and the crossover is sharp.

Pre-seeding 60 major Indian cities, measured with `seed --dry-run`:

| | Overpass seed | PBF extract |
|---|---|---|
| Work | **1,500 API calls** | 1 download |
| Time | **~12.5 hours** | ~15 min |
| Coverage | 60 cities, 37,525 km² | **all of India**, every town and village |
| Risk | 1,500 calls against a 2-slot community API — blocking is likely | none |
| Size | ~0 | 1.71 GB |

So: **use the PBF for bulk, lazy tiles for the long tail.** After a PBF
ingest, every tile that received places is marked in the tile cache, so the
API won't re-fetch ground it already holds — and areas outside the extract
still fill in lazily on demand.

```bash
# See the cost before committing to it
cargo run --release -p places --bin build-places -- seed --dry-run

# Bulk: whole country, offline (rebuilt daily by Geofabrik)
curl -L -o data/osm/india-latest.osm.pbf \
    https://download.geofabrik.de/asia/india-latest.osm.pbf
cargo run --release -p places --bin build-places -- \
    pbf --file data/osm/india-latest.osm.pbf

# Or seed specific cities via Overpass (rate-limited, resumable)
cargo run --release -p places --bin build-places -- \
    seed --cities crates/places/cities-india.json --max-tiles 100
```

The seed run saves its tile cache after **every** tile, so a multi-hour job
survives a crash or Ctrl-C and resumes where it stopped.

Alternatives if OSM coverage is thin for your category: Overture Maps
(open, backed by Meta/Microsoft/AWS), or Foursquare's open places dataset.
Google Places API works but costs per call and is the thing you're competing
with.

### 2. The user's location

Two sources, and you want both:

- **Browser Geolocation API** — `navigator.geolocation.getCurrentPosition()`.
  Accurate to ~10-50 m, requires an explicit permission prompt, and only
  works on HTTPS (or localhost).
- **IP geolocation** — no permission needed but only city-accurate, and
  wrong on VPNs and mobile carrier NAT. Use it as the fallback that makes
  the page useful before the user grants permission, not as the primary.

Never send precise coordinates anywhere you don't control, and don't log
them alongside queries without saying so.

### 3. A geospatial index

You cannot compute distance to five million places per query. The approach
here is **geohashing** (`crates/places/src/geo.rs`):

A geohash interleaves lat/lon bits into a base-32 string, so a **shared
prefix means spatial proximity** — everything in `tdr1y` sits in one box.
That turns "near me" into an ordinary inverted-index term lookup, and the
existing tantivy machinery handles it. No PostGIS required.

At index time each place stores every prefix from length 3 to 9. At query
time:

1. `cells_covering(lat, lon, radius)` → the cells overlapping the circle,
   **including the eight neighbours** (without them, a place 50 m away but
   across a cell boundary is invisible — a bug that makes search work
   "except sometimes").
2. OR those cells as terms → cheap candidate set.
3. **Exact haversine distance**, and drop anything outside the true radius —
   cells are squares, the query is a circle, so the corners are up to ~40%
   further than asked.
4. Rank.

Alternatives worth knowing: **S2** (Google's, better cell shapes) and **H3**
(Uber's, hexagonal so all neighbours are equidistant). Geohash is the
simplest thing that composes with an existing inverted index.

## Ranking

```
score = 0.8 × text_match          (normalised 0..1)
      + 1.0 × exp(-distance / (0.4 × radius))
      + 0.3 × prominence
```

Three deliberate choices:

**Proximity outweighs text.** On "cafe near me" every candidate is already a
cafe, so text carries almost no information and distance is what the user is
actually choosing on. This is the opposite of web-search weighting.

**Decay is exponential, not linear.** 100 m → 600 m is the difference
between "walk there" and "maybe not"; 9 km → 9.5 km is nothing. Linear
distance penalties get this backwards. Test:
`decay_punishes_near_distances_more_than_far_ones`.

**Decay scales with the radius**, so "within 500 m" and "within 20 km" both
produce sensible gradients instead of one of them going flat.

`prominence` is a completeness proxy — has website / phone / hours / address.
OSM carries no ratings, so this stands in for popularity. It's weak, and it's
the first thing to replace once you have real signals (reviews, check-ins, or
click-through from your own users).

## Intent detection

`"pizza"` and `"pizza near me"` want different indexes. `has_local_intent()`
looks for locality markers, and `strip_local_markers()` removes them before
text matching — otherwise you search for documents containing "near" and "me".

## Measured, on 3,139 real Bengaluru POIs

```
q=cafe, lat=12.9752, lon=77.6069, radius=1500m
  176 candidates -> 8 results in 0 ms

   20m  Cafe Levista       score 1.69
   66m  Cafe Azzure        score 1.69
   80m  Qube Cafe          score 1.60
   34m  Cafe Coffee Day    score 1.59
```

## Known limitations

- **Nodes only.** The PBF reader skips ways and relations, so places mapped
  as building polygons (many malls, hospitals, campuses) are missed. Fixing
  it means resolving way geometry to a centroid.
- **`prominence` is not popularity.** A meticulously-tagged corner shop
  outranks a famous restaurant nobody tagged. This is the biggest quality
  gap versus Google.
- **No opening-hours filtering.** `opening_hours` is stored but not parsed,
  so "open now" isn't supported. The OSM syntax is genuinely fiddly — use
  the `opening_hours` crate rather than writing a parser.
- **No geocoding.** "cafes in Koramangala" won't work; only lat/lon input is
  supported. You'd need a place-name → bounding-box lookup (Nominatim).
- **No road-distance or travel time** — straight-line only, which
  understates distance where rivers, railways or one-ways intervene.
- **Cold tiles are slow** — ~28 s for a dense urban tile. Acceptable once
  per area, poor as a first impression. Pre-seed the cities you care about.
- **Overpass is rate-limited** (2 concurrent slots) and unsuitable for bulk
  ingest. Fetches are serialised behind a lock so concurrent requests for
  the same cold area don't duplicate the call.
- **Tiles never expire from disk**, only refresh on TTL. A long-running
  instance with users everywhere eventually holds the whole country — at
  which point you should have ingested the PBF instead.

## Where this becomes a real product

Local is the most plausible place for a new entrant to beat Google in India,
because Google's weakness here is *data freshness on small businesses* —
hours, whether a place still exists, whether it delivers. That's won with
ground-level data collection, not better ranking. Which is the honest pitch:
the ranking stack is table stakes and this repo shows it works; the moat
would be the data operation.
