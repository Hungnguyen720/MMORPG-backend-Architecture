#!/usr/bin/env bash
# Run one tenant's full backend on this machine: login, world and one zone
# process hosting every zone (more layers open on demand).
#
#   scripts/run-stack.sh default            # ports 7000/7001, UDP 5000+
#   scripts/run-stack.sh ashfall 8000 6000  # ports 8000/8001, UDP 6000+
#
# Every tenant gets its own Postgres database (mmo_<tenant>) and its own
# Redis key prefix; the binaries are the same for every tenant.
# Env: MMO_DB_BASE (postgres://mmo:mmo@127.0.0.1), MMO_REDIS, MMO_NETCODE_KEY,
#      MMO_INTERNAL_KEY, LAYER_CAP (world --layer-cap), LOGS.
set -euo pipefail
cd "$(dirname "$0")/.."
tenant=${1:?usage: run-stack.sh <tenant> [http base port] [udp base port]}
http=${2:-7000}
udp=${3:-5000}
DB_BASE=${MMO_DB_BASE:-postgres://mmo:mmo@127.0.0.1}
DB="$DB_BASE/mmo_$tenant"
REDIS=${MMO_REDIS:-redis://127.0.0.1/}
KEY=${MMO_NETCODE_KEY:-$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')}
IKEY=${MMO_INTERNAL_KEY:-internal-$tenant-$RANDOM}
LOGS=${LOGS:-target/stack-$tenant}
BIN=${BIN:-target/release}
mkdir -p "$LOGS" build

$BIN/packc "tenants/$tenant" -o "build/$tenant.pack"
assets=()
if [[ -d tenants/$tenant/assets/tenant ]]; then
  scripts/build-assets.sh "$tenant" >/dev/null
  assets=(--assets "build/$tenant-assets.zip")
fi
psql "$DB_BASE/postgres" -tc "SELECT 1 FROM pg_database WHERE datname = 'mmo_$tenant'" | grep -q 1 \
  || psql "$DB_BASE/postgres" -qc "CREATE DATABASE mmo_$tenant"

COMMON=(--content "build/$tenant.pack" --db "$DB" --redis "$REDIS" --netcode-key "$KEY" --internal-key "$IKEY" --world "http://127.0.0.1:$((http + 1))")
zones=()
port=$udp
for z in $(python3 -c "import yaml,sys; print(' '.join(z['id'] for z in yaml.safe_load(open('tenants/$tenant/zones.yaml'))['zones']))"); do
  zones+=(--zone "$z:$port")
  port=$((port + 1))
done
cap=()
[[ -n ${LAYER_CAP:-} ]] && cap=(--layer-cap "$LAYER_CAP")

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT INT TERM
$BIN/world "${COMMON[@]}" --port $((http + 1)) "${cap[@]}" >"$LOGS/world.log" 2>&1 & pids+=($!)
$BIN/login "${COMMON[@]}" --port "$http" "${assets[@]}" >"$LOGS/login.log" 2>&1 & pids+=($!)
$BIN/zone-server "${COMMON[@]}" "${zones[@]}" --metrics $((http + 100)) --control $((http + 101)) \
  --ports $((udp + 100))-$((udp + 199)) --name "$tenant-zone-1" >"$LOGS/zone.log" 2>&1 & pids+=($!)
echo "$tenant: login http://127.0.0.1:$http  world :$((http + 1))  zones ${zones[*]}  metrics :$((http + 100))  logs $LOGS"
wait
