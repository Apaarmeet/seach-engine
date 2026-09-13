#!/usr/bin/env bash
# Weight sweep: grid-search the ranking weights against the judgment set.
#
# This is the honest way to choose weights. Hand-picking numbers until your
# favourite query looks right is how you overfit to one query and regress
# twenty others — the sweep makes the tradeoff visible instead.
set -uo pipefail
cd "$(dirname "$0")/.."

API=./target/release/api
EVAL=./target/release/eval
PORT=8098

measure() {
  pkill -f "release/api --port $PORT" 2>/dev/null
  sleep 0.3
  $API --port $PORT "$@" >/dev/null 2>&1 &
  local pid=$!
  for _ in $(seq 1 40); do
    curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 && break
    sleep 0.25
  done
  $EVAL --api "http://localhost:$PORT" 2>/dev/null | grep '^MEAN' | awk '{print $2, $3}'
  kill $pid 2>/dev/null
  wait $pid 2>/dev/null
}

printf "%8s %8s %8s %10s %8s\n" "PAGERANK" "QUALITY" "TRUST" "NDCG@10" "MRR"
printf -- "----------------------------------------------------\n"

best_ndcg=0
best_cfg=""

for pr in 0.0 0.3 0.6 1.0 1.5; do
  for tr in 0.0 0.3 0.6; do
    read -r ndcg mrr <<<"$(measure --pagerank-weight "$pr" --quality-weight 0.2 --domain-trust-weight "$tr")"
    printf "%8s %8s %8s %10s %8s\n" "$pr" "0.2" "$tr" "${ndcg:-ERR}" "${mrr:-ERR}"
    if [ -n "${ndcg:-}" ] && awk "BEGIN{exit !($ndcg > $best_ndcg)}" 2>/dev/null; then
      best_ndcg=$ndcg
      best_cfg="pagerank=$pr trust=$tr"
    fi
  done
done

printf -- "----------------------------------------------------\n"
echo "best: $best_cfg  ->  NDCG@10 $best_ndcg"
