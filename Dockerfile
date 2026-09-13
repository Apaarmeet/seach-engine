# Single-binary deployment: the Rust API serves the API *and* the built
# frontend, so there is one process, one port, and no CORS surface.
#
# The indexes are baked into the image rather than built at deploy time or
# mounted from a volume. Building them needs the 1.7 GB OSM extract and two
# full passes over 291M elements — far too slow for a deploy step — and a
# volume adds a stateful component to what is otherwise an immutable,
# redeployable artifact. ~250 MB of index in the image is the cheaper trade.

# ---- Stage 1: build the frontend ----
FROM node:22-slim AS frontend
WORKDIR /app
COPY frontend/package.json frontend/package-lock.json* ./
# `npm ci` when a lockfile exists, `npm install` otherwise.
RUN npm ci 2>/dev/null || npm install
COPY frontend/ ./
RUN npm run build

# ---- Stage 2: build the API ----
FROM rust:1.98-slim AS backend
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first so dependency compilation caches across source edits.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --release -p api

# ---- Stage 3: runtime ----
FROM debian:bookworm-slim
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=backend /build/target/release/api /usr/local/bin/api
COPY --from=frontend /app/dist ./static

# Prebuilt indexes. Regenerate locally with:
#   cargo run --release -p places --bin build-places -- pbf --file <extract>
#   cargo run --release -p signals && cargo run --release -p indexer --bin build-index
COPY places-index/ ./places-index/
COPY index/ ./index/

EXPOSE 8080

# --no-lazy-fetch: the PBF already covers India, so the server must never
# call Overpass at request time. Without this a query near a region boundary
# could block for ~28s on an external API during a demo.
CMD ["api", \
     "--port", "8080", \
     "--index-dirs", "/app/index", \
     "--places-dir", "/app/places-index", \
     "--static-dir", "/app/static", \
     "--no-lazy-fetch"]
