#!/usr/bin/env bash
# Ablation study: measure what each ranking signal is actually worth.
#
# Turn one signal off at a time, re-measure NDCG@10, and see how far quality
# falls. This is how you justify every term in the scoring function — and
# how you discover that a signal you were proud of contributes nothing.
set -uo pipefail
cd "$(dirname "$0")/.."

API=./target/release/api
EVAL=./target/release/eval
PORT=8099

run_config() {
  local label="$1"; shift
  pkill -f "release/api --port $PORT" 2>/dev/null
  sleep 0.4
  $API --port $PORT "$@" >/dev/null 2>&1 &
  local pid=$!
  # Wait for readiness rather than sleeping a fixed amount.
  for _ in $(seq 1 40); do
    curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 && break
    sleep 0.25
  done
  local out
  out=$($EVAL --api "http://localhost:$PORT" 2>/dev/null | grep '^MEAN')
  local ndcg rr
  ndcg=$(awk '{print $2}' <<<"$out")
  rr=$(awk '{print $3}' <<<"$out")
  printf "%-34s %8s %8s\n" "$label" "${ndcg:-ERR}" "${rr:-ERR}"
  kill $pid 2>/dev/null
  wait $pid 2>/dev/null
}

printf "%-34s %8s %8s\n" "CONFIGURATION" "NDCG@10" "MRR"
printf -- "------------------------------------------------------\n"

run_config "full ranking (all signals)"
run_config "  without anchor text"        --anchor-boost 0
run_config "  without link authority"     --pagerank-weight 0
run_config "  without quality signal"     --quality-weight 0
run_config "  without domain trust"       --domain-trust-weight 0
run_config "  text relevance only"        --anchor-boost 0 --pagerank-weight 0 \
                                          --quality-weight 0 --domain-trust-weight 0

printf -- "------------------------------------------------------\n"
echo "Each row disables one signal. The drop from row 1 is that"
echo "signal's measured contribution on this judgment set."
