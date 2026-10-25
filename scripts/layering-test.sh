#!/usr/bin/env bash
# Layering check against a running stack (scripts/run-stack.sh) started with
# a small LAYER_CAP and a high MMO_LOGIN_RATE, e.g.:
#   LAYER_CAP=50 MMO_LOGIN_RATE=100000 MMO_INTERNAL_KEY=k scripts/run-stack.sh default
#   MMO_INTERNAL_KEY=k scripts/layering-test.sh
# A crowd of 100 short-lived and 30 long-lived bots logs in: the world opens
# extra layers. The crowd leaves; the world merges the 30 that remain onto
# fewer layers by handing them off, and the empty layers close.
set -euo pipefail
cd "$(dirname "$0")/.."
LOGIN=${LOGIN:-http://127.0.0.1:7000}
WORLD=${WORLD:-http://127.0.0.1:7001}
IKEY=${MMO_INTERNAL_KEY:?set MMO_INTERNAL_KEY to the stack internal key}
OUT=${OUT:-docs/load-tests}
mkdir -p "$OUT"
status() {
  curl -s -H "x-internal-key: $IKEY" "$WORLD/status" | python3 scripts/layers.py
}
timeline=$OUT/phase3-layering-timeline.txt
: >"$timeline"
(t0=$(date +%s); while true; do echo "t+$(( $(date +%s) - t0 ))s  $(status)" | tee -a "$timeline"; sleep 5; done) &
watcher=$!
trap 'kill $watcher 2>/dev/null || true' EXIT

target/release/swarm --login "$LOGIN" --bots 100 --ramp-per-sec 20 --seconds 30 --roam 60 \
  --max-tick-p99 20 --json "$OUT/phase3-layering-crowd.json" >"$OUT/phase3-layering-crowd.txt" 2>&1 &
crowd=$!
target/release/swarm --login "$LOGIN" --bots 30 --ramp-per-sec 20 --seconds 120 --roam 60 \
  --max-tick-p99 20 --json "$OUT/phase3-layering-stay.json" >"$OUT/phase3-layering-stay.txt" 2>&1 &
stay=$!
wait $crowd || true
wait $stay || true
sleep 5
echo "final: $(status)" | tee -a "$timeline"
grep -E "connected at end|handoffs|progress" "$OUT"/phase3-layering-*.txt
