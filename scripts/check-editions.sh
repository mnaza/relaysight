#!/usr/bin/env bash
# Prove that the two editions really answer differently, headless.
#
# Brings the API up three times — no entitlement service, then the stand-in
# serving hosted-free, then serving commercial-pro — seeds five cameras into
# each with scripts/demo-fleet.sh, and checks what the API says about the plan
# and how many cameras it kept.
#
#   make check-editions
set -euo pipefail

cd "$(dirname "$0")/.."

API_PORT="${API_PORT:-18085}"
ENTITLEMENTS_PORT="${ENTITLEMENTS_PORT:-18089}"
ADMIN_PASSWORD="${ADMIN_PASSWORD:-demo-admin-password}"
CAMERAS=5
CONTAINER="check-editions-entitlements"
API_URL="http://127.0.0.1:${API_PORT}"

workdir="$(mktemp -d)"
api_pid=""
failures=0

cleanup() {
  if [[ -n "$api_pid" ]]; then
    kill "$api_pid" 2>/dev/null || true
    wait "$api_pid" 2>/dev/null || true
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  rm -rf "$workdir"
}
trap cleanup EXIT

port_taken() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
for port in "$API_PORT" "$ENTITLEMENTS_PORT"; do
  if port_taken "$port"; then
    echo "port ${port} is in use; set API_PORT/ENTITLEMENTS_PORT to something free" >&2
    exit 1
  fi
done

wait_for() {
  local url="$1" tries=100
  until curl -fsS "$url" >/dev/null 2>&1; do
    tries=$((tries - 1))
    [[ $tries -gt 0 ]] || { echo "timed out waiting for ${url}" >&2; return 1; }
    sleep 0.2
  done
}

echo "building the API..."
cargo build -q -p vms-api

start_entitlements() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run --rm -d --name "$CONTAINER" \
    -e "DEMO_PLAN=$1" \
    -v "$PWD/deploy/demo-entitlements:/srv/entitlements:ro" \
    -v "$PWD/deploy/demo-entitlements/entitlements.conf.template:/etc/nginx/templates/entitlements.conf.template:ro" \
    -p "127.0.0.1:${ENTITLEMENTS_PORT}:8088" \
    nginx:1.27-alpine >/dev/null
  wait_for "http://127.0.0.1:${ENTITLEMENTS_PORT}/healthz"
}

field() { curl -fsS "${API_URL}/api/v1/system/edition" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"$1\"])"; }

check() {
  local name="$1" entitlements_url="$2" want_edition="$3" want_limit="$4" want_kept="$5"
  echo
  echo "--- ${name}"
  ADMIN_PASSWORD="$ADMIN_PASSWORD" \
  API_BIND="127.0.0.1:${API_PORT}" \
  DATABASE_URL="sqlite:${workdir}/${name}.db" \
  ENTITLEMENTS_URL="$entitlements_url" \
  PLUGIN_DIR="plugins.d" \
  RUST_LOG="warn" \
    ./target/debug/vms-api >"${workdir}/${name}.api.log" 2>&1 &
  api_pid=$!
  wait_for "${API_URL}/healthz"

  API_URL="$API_URL" ADMIN_PASSWORD="$ADMIN_PASSWORD" CAMERAS="$CAMERAS" \
    GATEWAY_ID="demo-gateway-${name}" \
    ./scripts/demo-fleet.sh >"${workdir}/${name}.seed.log"

  local edition limit kept
  edition="$(field edition)"
  limit="$(field camera_limit)"
  kept="$(sed -n 's/^cameras in fleet: //p' "${workdir}/${name}.seed.log")"
  printf 'edition=%s camera_limit=%s cameras_kept=%s (sent %s)\n' \
    "$edition" "$limit" "$kept" "$CAMERAS"

  local expected="${want_edition}/${want_limit}/${want_kept}"
  local actual="${edition}/${limit}/${kept}"
  if [[ "$actual" == "$expected" ]]; then
    echo "ok"
  else
    echo "FAILED: expected ${expected}, got ${actual}" >&2
    failures=$((failures + 1))
  fi

  kill "$api_pid" 2>/dev/null || true
  wait "$api_pid" 2>/dev/null || true
  api_pid=""
}

check community "" community None "$CAMERAS"

start_entitlements hosted-free
check hosted-free "http://127.0.0.1:${ENTITLEMENTS_PORT}" commercial 3 3

start_entitlements pro
check pro "http://127.0.0.1:${ENTITLEMENTS_PORT}" commercial None "$CAMERAS"

echo
if [[ $failures -eq 0 ]]; then
  echo "All three editions answered as expected."
else
  echo "${failures} of 3 editions answered differently than expected." >&2
  exit 1
fi
