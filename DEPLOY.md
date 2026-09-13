# Deploying rsearch

The goal here is a link that works when someone opens it days after you sent
it — which rules out anything that sleeps when idle or depends on your laptop.

## What ships

One binary serving both the API and the frontend, with the indexes baked
into the image:

| | |
|---|---|
| `api` binary | 12 MB |
| `places-index/` | 144 MB (649,740 places + 312k place names) |
| `index/` | 108 MB (web demo corpus) |
| frontend `dist/` | ~170 KB |
| **image** | **~400 MB** |

Indexes are in the image rather than on a volume so the deployment stays a
single immutable artifact. Rebuilding them needs the 1.7 GB OSM extract and
two passes over 291M elements — far too slow for a deploy step.

## Railway

Railway builds from your repo and assigns a port at runtime. Two things
that trip this project up specifically:

**1. The port is dynamic.** Railway sets `$PORT` and routes only to that.
The binary reads it from the environment (`--port` would override it, so the
Dockerfile deliberately omits the flag). Hardcoding 8080 produces a deploy
that reports healthy while the public URL 502s.

**2. The indexes are not in git.** `places-index/` (144 MB) and `index/`
(108 MB) are gitignored, but the Dockerfile copies them into the image. A
repo-based build therefore fails on `COPY places-index/`. `.railwayignore`
exists to fix this: Railway prefers it over `.gitignore`, and it permits the
index directories while still excluding `target/` and `data/`.

So deploy with the CLI from your working directory, not from a GitHub
integration:

```bash
npm i -g @railway/cli
railway login
railway init            # creates the project
railway up              # uploads ~250 MB of index, then builds
railway domain          # generates the public URL
```

Verify before sending the link:

```bash
curl -s https://<your-app>.up.railway.app/health          # -> ok
curl -s https://<your-app>.up.railway.app/places-stats     # -> 649740
```

Railway has no free tier (trial credit only) and no India region, so expect
~200 ms of added latency from India and a few dollars a month.

## Why Fly.io

| Option | Verdict |
|---|---|
| **Fly.io** | ~$5/mo, Mumbai region, no cold starts. **Recommended.** |
| Render free tier | Spins down when idle — first visit after a quiet period waits ~60s. Reads as broken. |
| Railway | Fine, but no free tier and no India region. |
| Cloudflare Tunnel | Free, but only works while your laptop is on. Fine for a scheduled live demo, wrong for an emailed link. |
| VPS (Hetzner ~€4/mo) | Cheapest long-run, most setup. |

`primary_region = "bom"` puts it in Mumbai — closest to both the data and
the audience.

## Deploy

```bash
# 1. Install flyctl and sign in (card required for verification; the
#    shared-cpu-1x/1GB machine runs a few dollars a month)
curl -L https://fly.io/install.sh | sh
fly auth signup        # or: fly auth login

# 2. From the repo root — fly.toml is already written
cd ~/Developer/search-engine
fly launch --no-deploy --copy-config --name rsearch-india

# 3. Ship it (first build ~10 min: it compiles Rust and uploads ~400 MB)
fly deploy

# 4. Confirm
fly status
fly logs
open https://rsearch-india.fly.dev
```

If `rsearch-india` is taken, pick another name and change `app` in
`fly.toml` to match.

## Before you send the link

```bash
curl -s https://rsearch-india.fly.dev/health                    # -> ok
curl -s https://rsearch-india.fly.dev/places-stats               # -> 649740 places
curl -s 'https://rsearch-india.fly.dev/nearby?q=hospitals+in+chennai&lat=13.08&lon=80.27&limit=3'
```

Then open it in a **private window** — that is what the recipient sees, with
no geolocation permission already granted. Confirm the Bengaluru fallback
renders and "use my location" prompts correctly.

## Things that will bite

- **Geolocation needs HTTPS.** Fly gives you that automatically
  (`force_https = true`). It would silently fail over plain HTTP.
- **Memory.** The gazetteer is a 31 MB JSON file parsed into a map of 312k
  places, and tantivy mmaps ~250 MB. 512 MB leaves no headroom for the parse
  spike at startup; `fly.toml` asks for 1 GB.
- **`--no-lazy-fetch` is set in the Dockerfile.** The PBF already covers
  India, so the server must never call Overpass while serving a request —
  a cold tile takes ~28s and would stall a demo.
- **Rust version.** The image pins `rust:1.98-slim`; a dependency needs
  `edition2024`, which 1.83 rejects.

## Updating the indexes

The image is immutable, so refreshing data means rebuilding and redeploying:

```bash
curl -L -o data/osm/india-latest.osm.pbf \
    https://download.geofabrik.de/asia/india-latest.osm.pbf
cargo run --release -p places --bin build-places -- \
    pbf --file data/osm/india-latest.osm.pbf
fly deploy
```

Geofabrik rebuilds the extract daily; monthly is plenty for POI data.

## Cheaper alternative for a scheduled live demo

If he is going to look at it on a call rather than on his own time:

```bash
brew install cloudflared
cloudflared tunnel --url http://localhost:8080
```

Prints a public HTTPS URL immediately, free, no account. Dies when you close
the terminal — which is exactly why it is unsuitable for an emailed link.
