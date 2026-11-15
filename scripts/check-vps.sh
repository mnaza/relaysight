#!/usr/bin/env bash
# Bring the VPS bundle up on this machine the way a fresh server would, and
# fail unless it serves what a person on the other end needs.
#
#   make check-vps
#
# deploy/vps/ is what runs on a rented box: Caddy with a real certificate in
# front of the dashboard and the API, the API on loopback, a gateway with an
# RTMP ingest, and test-pattern cameras pushing into it. Here DOMAIN is
# localhost, so Caddy issues from its own internal CA instead of Let's
# Encrypt, and every port is a free one rather than 80 and 443.
#
# What it proves: the bundle starts from nothing with one script; the
# dashboard and the API answer over TLS through Caddy and plain HTTP
# redirects; the session cookie is Secure; the API and the RTMP ingest are on
# loopback only; the device tunnel is off; every demo camera turns healthy;
# and a real Firefox decodes frames from one of them.
#
# What it cannot prove: the browser here reaches the gateway over loopback.
# On the VPS it reaches it over the public address in RTC_UDP_ADDRS, through
# whatever firewall the provider puts in front — that path is proved only by
# opening the dashboard from somewhere else (docs/DEPLOY-VPS.md).
set -euo pipefail

cd "$(dirname "$0")/.."

need() {
    command -v "$1" >/dev/null || { echo "$1 is not on PATH; $2" >&2; exit 1; }
}
need docker "the bundle is compose"
need curl "it is how this check talks to the stack"
need ss "it is how this check sees what is listening where"

[ -x deploy/vps/up.sh ] || { echo "deploy/vps/up.sh is missing or not executable" >&2; exit 1; }

work="$(mktemp -d)"
project="relaysight-vps-check"
compose() {
    docker compose -p "$project" --env-file "$work/.env" -f deploy/vps/docker-compose.yml "$@"
}
down() {
    compose down -v --remove-orphans >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap down EXIT

free_port() {
    python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
}

wait_for() {
    local what="$1" tries="${2:-60}"
    for _ in $(seq 1 "$tries"); do
        eval "$what" >/dev/null 2>&1 && return 0
        sleep 2
    done
    return 1
}

HTTP_PORT="$(free_port)"; HTTPS_PORT="$(free_port)"; API_PORT="$(free_port)"; RTMP_PORT="$(free_port)"
CAMERAS=2
cat > "$work/.env" <<ENV
COMPOSE_PROJECT_NAME=$project
DOMAIN=localhost
PUBLIC_IP=
HTTP_PORT=$HTTP_PORT
HTTPS_PORT=$HTTPS_PORT
API_PORT=$API_PORT
RTMP_PORT=$RTMP_PORT
DEMO_CAMERAS=$CAMERAS
RTC_STUN_URLS=
ENV

echo "1/7 bringing the bundle up from nothing (the first build takes a while)"
ENV_FILE="$work/.env" deploy/vps/up.sh 2>&1 | sed 's/^/    /'
[ "${PIPESTATUS[0]}" -eq 0 ] || { echo "up.sh failed" >&2; exit 1; }

# shellcheck disable=SC1091
. "$work/.env"
[ -n "${ADMIN_PASSWORD:-}" ] || { echo "up.sh did not write an admin password into the env file" >&2; exit 1; }
[ -n "${GATEWAY_TOKEN:-}" ] || { echo "up.sh did not write a gateway token into the env file" >&2; exit 1; }
[ "$GATEWAY_TOKEN" != "demo-local-token" ] || { echo "the gateway token is the public demo default" >&2; exit 1; }

# up.sh is also how a server is upgraded, so a second run must keep the
# secrets it made and register nothing twice.
first_password="$ADMIN_PASSWORD"
again="$(ENV_FILE="$work/.env" deploy/vps/up.sh 2>&1)" || { echo "a second up.sh failed:" >&2; echo "$again" >&2; exit 1; }
if echo "$again" | grep -qE '^(generated|registered) '; then
    echo "a second up.sh changed what the first one set up:" >&2
    echo "$again" | grep -E '^(generated|registered) ' >&2
    exit 1
fi
. "$work/.env"
[ "$ADMIN_PASSWORD" = "$first_password" ] || { echo "a second up.sh replaced the admin password" >&2; exit 1; }
echo "    a second run kept the secrets and registered nothing twice"

SITE="https://localhost:$HTTPS_PORT"
CURL=(curl -fsS -k --max-time 10)

echo "2/7 the dashboard and the API over TLS, through Caddy"
wait_for "${CURL[*]} -o /dev/null $SITE/healthz" 60 || { echo "no answer over TLS on $HTTPS_PORT" >&2; exit 1; }
"${CURL[@]}" "$SITE/" | grep -q '<title>' || { echo "the dashboard page did not come back from $SITE/" >&2; exit 1; }
redirect="$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "http://localhost:$HTTP_PORT/")"
case "$redirect" in
    30?\ https://*) echo "    plain HTTP redirects: $redirect" ;;
    *) echo "plain HTTP should redirect to https, got: $redirect" >&2; exit 1 ;;
esac

echo "3/7 logging in over TLS, and the cookie it sets"
headers="$("${CURL[@]}" -D - -o /dev/null -c "$work/cookie" -H 'content-type: application/json' \
    -d "{\"email\":\"${ADMIN_EMAIL:-admin@localhost}\",\"password\":\"$ADMIN_PASSWORD\"}" \
    "$SITE/api/v1/auth/login")"
echo "$headers" | grep -i '^set-cookie:' | grep -qi 'secure' \
    || { echo "the session cookie is not Secure on a TLS deployment" >&2; echo "$headers" >&2; exit 1; }

echo "4/7 nothing but Caddy on a public interface"
listening="$(ss -ltnH | awk '{print $4}')"
for port in "$API_PORT" "$RTMP_PORT"; do
    bound="$(echo "$listening" | grep -E ":$port\$" || true)"
    [ -n "$bound" ] || { echo "nothing listens on $port at all" >&2; exit 1; }
    if echo "$bound" | grep -vqE '^(127\.0\.0\.1|\[::1\]):'; then
        echo "port $port is listening beyond loopback: $bound" >&2
        exit 1
    fi
done
gw="$(compose ps -q gateway)"
if docker inspect "$gw" --format '{{range .Config.Env}}{{println .}}{{end}}' | grep -qx 'TUNNEL_ENABLED=true'; then
    echo "the demo gateway allows the device tunnel" >&2; exit 1
fi
echo "    API $API_PORT and RTMP $RTMP_PORT on loopback, tunnel off"

echo "5/7 every demo camera turns healthy"
healthy() {
    "${CURL[@]}" -b "$work/cookie" "$SITE/api/v1/cameras" | python3 -c "
import json, sys
print(sum(1 for c in json.load(sys.stdin) if c.get('status') == 'healthy'))"
}
wait_for "[ \"\$(healthy)\" -ge $CAMERAS ]" 90 || {
    echo "only $(healthy) of $CAMERAS demo cameras are healthy" >&2
    "${CURL[@]}" -b "$work/cookie" "$SITE/api/v1/cameras" >&2 || true
    compose logs --tail 20 gateway demo-cameras >&2 || true
    exit 1
}
echo "    $(healthy) healthy"

echo "6/7 the ICE configuration the browser will be handed"
"${CURL[@]}" -b "$work/cookie" "$SITE/api/v1/rtc/config" | grep -qiE 'ice_?servers' \
    || { echo "rtc/config did not answer an ICE configuration" >&2; exit 1; }

echo "7/7 a real Firefox decodes frames from a demo camera"
LIVE_API="http://127.0.0.1:$API_PORT" LIVE_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
    ./scripts/check-live.sh 2>&1 | sed 's/^/    /'
[ "${PIPESTATUS[0]}" -eq 0 ] || { echo "no frame reached the browser" >&2; exit 1; }

echo
echo "the VPS bundle came up from nothing and carried live video"
