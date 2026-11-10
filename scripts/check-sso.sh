#!/usr/bin/env bash
# Sign in through an identity provider, over the network, between processes.
#
# The API's own tests start the fake provider in the test process and drive
# the handlers through the router. This starts three separate things — a
# provider, the control plane, and curl as the browser — and lets them talk
# over HTTP. What it proves that the tests cannot is that the redirects, the
# cookie and the token exchange survive being real.
set -euo pipefail

cd "$(dirname "$0")/.."

free_port() {
    python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
}

work="$(mktemp -d)"
provider_pid=""
api_pid=""
cleanup() {
    [ -n "$api_pid" ] && kill "$api_pid" 2>/dev/null || true
    [ -n "$provider_pid" ] && kill "$provider_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

echo "== building"
cargo build -q -p vms-fake-oidc -p vms-api

wait_for() {
    local url="$1" name="$2"
    for _ in $(seq 1 100); do
        if curl -fs -o /dev/null "$url"; then return 0; fi
        sleep 0.2
    done
    echo "$name never answered at $url" >&2
    exit 1
}

start_provider() {  # port, email
    ./target/debug/vms-fake-oidc "$1" "$2" > "$work/provider-$1.log" 2>&1 &
    provider_pid=$!
    wait_for "http://127.0.0.1:$1/.well-known/openid-configuration" "the provider"
}

start_api() {  # port, issuer
    API_BIND="127.0.0.1:$1" \
    DATABASE_URL="sqlite:$work/vms.db" \
    ADMIN_PASSWORD="a long enough password" \
    ADMIN_EMAIL="admin@localhost" \
    PLUGIN_DIR="$work/plugins.d" \
    OIDC_ISSUER="$2" \
    OIDC_CLIENT_ID="relaysight-test" \
    OIDC_CLIENT_SECRET="relaysight-test-secret" \
    OIDC_REDIRECT_URL="http://127.0.0.1:$1/api/v1/auth/oidc/callback" \
        ./target/debug/vms-api > "$work/api-$1.log" 2>&1 &
    api_pid=$!
    wait_for "http://127.0.0.1:$1/healthz" "the API"
}

mkdir -p "$work/plugins.d"

provider_port="$(free_port)"
api_port="$(free_port)"
start_provider "$provider_port" "admin@localhost"
start_api "$api_port" "http://127.0.0.1:$provider_port"
api="http://127.0.0.1:$api_port"

echo "== the login page is offered the provider"
methods="$(curl -fsS "$api/api/v1/auth/methods")"
echo "$methods" | grep -q '"sso":true' || { echo "the API does not offer SSO: $methods" >&2; exit 1; }

echo "== a round trip: start, authorize, callback"
jar="$work/cookies"
curl -sS -o /dev/null -c "$jar" -b "$jar" -L --max-redirs 5 "$api/api/v1/auth/oidc/start"
grep -q "vms_session" "$jar" || { echo "no session cookie came out of the round trip" >&2; cat "$jar" >&2; exit 1; }

code="$(curl -sS -o /dev/null -w '%{http_code}' -b "$jar" "$api/api/v1/fleet")"
[ "$code" = "200" ] || { echo "the session the provider minted does not work: $code" >&2; exit 1; }
echo "   signed in as admin@localhost, and the session reads the fleet"

echo "== a callback this system did not start is refused"
code="$(curl -sS -o /dev/null -w '%{http_code}' "$api/api/v1/auth/oidc/callback?code=made-up&state=made-up")"
[ "$code" = "401" ] || { echo "an invented callback answered $code" >&2; exit 1; }

echo "== an email with no account here is refused"
kill "$api_pid"; wait "$api_pid" 2>/dev/null || true; api_pid=""
kill "$provider_pid"; wait "$provider_pid" 2>/dev/null || true; provider_pid=""
provider_port="$(free_port)"
api_port="$(free_port)"
start_provider "$provider_port" "stranger@example.test"
start_api "$api_port" "http://127.0.0.1:$provider_port"
api="http://127.0.0.1:$api_port"

stranger="$work/stranger-cookies"
code="$(curl -sS -o /dev/null -w '%{http_code}' -c "$stranger" -b "$stranger" -L --max-redirs 5 \
    "$api/api/v1/auth/oidc/start")"
[ "$code" = "401" ] || { echo "a stranger the provider vouched for was answered $code" >&2; exit 1; }
grep -q "vms_session" "$stranger" && { echo "a refused sign-in still set a session cookie" >&2; exit 1; }

echo
echo "SSO round trip: a provider, a control plane and a browser, over HTTP."
