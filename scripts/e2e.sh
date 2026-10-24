#!/usr/bin/env bash
# Phase 2 done check, end to end on one machine:
#   log in, level a character across two zones, log out, log back in and
#   compare; then loot an item, SIGKILL the zone process holding the
#   character, restart it, and verify nothing was lost.
# Needs Postgres and Redis running. Uses the release build.
set -euo pipefail
cd "$(dirname "$0")/.."

DB_URL=${MMO_DB:-postgres://mmo:mmo@127.0.0.1/mmo_e2e}
REDIS=${MMO_REDIS:-redis://127.0.0.1/}
KEY=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
IKEY=e2e-internal-$RANDOM
LOGS=${LOGS:-target/e2e-logs}
mkdir -p "$LOGS"
BIN=target/release
COMMON=(--content tenants/default --db "$DB_URL" --redis "$REDIS" --netcode-key "$KEY" --internal-key "$IKEY")

if [[ "${FRESH:-1}" == 1 ]]; then
  psql "${DB_URL%/*}/postgres" -qc "DROP DATABASE IF EXISTS ${DB_URL##*/}" -c "CREATE DATABASE ${DB_URL##*/}"
  redis-cli -u "$REDIS" --scan --pattern 'default:*' | xargs -r redis-cli -u "$REDIS" del >/dev/null
fi

pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

$BIN/world "${COMMON[@]}" --port 7001 >"$LOGS/world.log" 2>&1 & pids+=($!)
$BIN/login "${COMMON[@]}" --port 7000 >"$LOGS/login.log" 2>&1 & pids+=($!)
# Two zone processes: Elderwood and the Hollow Crypt.
start_zone_a() { $BIN/zone-server "${COMMON[@]}" --zone elderwood:5000 --metrics 9100 --control 7100 --name zone-a --ports 5100-5149 >>"$LOGS/zone-a.log" 2>&1 & ZA=$!; pids+=($ZA); }
start_zone_b() { $BIN/zone-server "${COMMON[@]}" --zone hollow_crypt:5001 --metrics 9101 --control 7101 --name zone-b --ports 5150-5199 >>"$LOGS/zone-b.log" 2>&1 & ZB=$!; pids+=($ZB); }
start_zone_a; start_zone_b
sleep 2

E2E=(--login http://127.0.0.1:7000 --email "e2e$RANDOM@example.com" --name Testrunner --state "$LOGS/state.json")

echo "== 1. log in, level across two zones, log out"
$BIN/e2e level "${E2E[@]}" | tee "$LOGS/level.log"
echo "== 2. log back in: everything is where we left it"
$BIN/e2e verify "${E2E[@]}" | tee "$LOGS/verify1.log"
echo "== 3. loot an item, then SIGKILL the crypt zone process"
$BIN/e2e loot-hold "${E2E[@]}" --state "$LOGS/state2.json" | tee "$LOGS/loot.log" &
HOLD=$!
for _ in $(seq 1 600); do grep -q HOLDING "$LOGS/loot.log" 2>/dev/null && break; sleep 0.5; done
echo "   kill -9 zone-b (pid $ZB)"
kill -9 "$ZB"
wait $HOLD || true
start_zone_b
sleep 2
echo "== 4. log back in after the crash: the looted item is still there"
$BIN/e2e verify "${E2E[@]}" --state "$LOGS/state2.json" | tee "$LOGS/verify2.log"
echo "== economy log (rare drops written the moment they dropped)"
psql "$DB_URL" -c "SELECT event, item, count, at FROM economy_log ORDER BY at" || true
echo "E2E PASS"
