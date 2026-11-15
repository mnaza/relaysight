#!/usr/bin/env bash
# Bring the server up, or bring it up to date: docs/DEPLOY-VPS.md.
#
#   cp .env.example .env    # DOMAIN and PUBLIC_IP
#   ./up.sh
#
# Safe to run again. It generates the owner's password and the gateway token
# into .env only when they are missing, builds and starts the stack, waits for
# the server's own gateway, and registers the demo cameras it does not have
# yet. ENV_FILE points it at another env file.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
env_file="${ENV_FILE:-$here/.env}"
[ -f "$env_file" ] || { echo "no $env_file: copy .env.example to it and set DOMAIN and PUBLIC_IP" >&2; exit 1; }

secret() { head -c 48 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 32; }
ensure() {
    local name="$1"
    if ! grep -qE "^$name=.+" "$env_file"; then
        local value; value="$(secret)"
        if grep -qE "^#?$name=" "$env_file"; then
            sed -i -E "s|^#?$name=.*|$name=$value|" "$env_file"
        else
            echo "$name=$value" >> "$env_file"
        fi
        echo "generated $name into $env_file"
    fi
}
ensure ADMIN_PASSWORD
ensure GATEWAY_TOKEN
chmod 600 "$env_file"

set -a
# shellcheck disable=SC1090
. "$env_file"
set +a
[ -n "${DOMAIN:-}" ] || { echo "DOMAIN is empty in $env_file" >&2; exit 1; }
if [ "$DOMAIN" != localhost ] && [ -z "${PUBLIC_IP:-}" ]; then
    echo "PUBLIC_IP is empty in $env_file; live video needs the address browsers reach this server at" >&2
    exit 1
fi

compose() { docker compose --env-file "$env_file" -f "$here/docker-compose.yml" "$@"; }
compose up -d --build

API="http://127.0.0.1:${API_PORT:-8080}"
wait_for() {
    local what="$1" tries="${2:-60}"
    for _ in $(seq 1 "$tries"); do
        eval "$what" >/dev/null 2>&1 && return 0
        sleep 2
    done
    return 1
}
wait_for "curl -fs -o /dev/null $API/healthz" 60 || { echo "the API did not come up" >&2; compose logs --tail 20 api >&2; exit 1; }

cookie="$(mktemp)"
trap 'rm -f "$cookie"' EXIT
login_body="$(python3 -c 'import json, os; print(json.dumps({"email": os.environ.get("ADMIN_EMAIL") or "admin@localhost", "password": os.environ["ADMIN_PASSWORD"]}))')"
curl -fs -c "$cookie" -H 'content-type: application/json' -d "$login_body" -o /dev/null "$API/api/v1/auth/login" \
    || { echo "could not log in as the owner; if .env changed after the first start, see docs/DEPLOY-VPS.md" >&2; exit 1; }

wait_for "curl -fs -b $cookie $API/api/v1/gateways | grep -q demo-gateway" 60 \
    || { echo "the server's own gateway never reported in" >&2; compose logs --tail 20 gateway >&2; exit 1; }

existing="$(curl -fs -b "$cookie" "$API/api/v1/sources")"
for i in $(seq 1 "${DEMO_CAMERAS:-3}"); do
    if ! echo "$existing" | grep -q "\"demo-$i\""; then
        curl -fs -b "$cookie" -H 'content-type: application/json' \
            -d "{\"gateway_id\":\"demo-gateway\",\"name\":\"Demo camera $i\",\"kind\":\"rtmp\",\"address\":\"demo-$i\"}" \
            -o /dev/null "$API/api/v1/sources"
        echo "registered Demo camera $i"
    fi
done

echo
echo "up: https://$DOMAIN/ — sign in as ${ADMIN_EMAIL:-admin@localhost}, password in $env_file"
