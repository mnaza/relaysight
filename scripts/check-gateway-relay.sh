#!/usr/bin/env bash
# The gateway's side of TURN over TLS, end to end: a local coturn with a TLS
# listener and a throwaway certificate authority, the gateway's UDP TURN URL
# pointing at a closed port so its probe fails, and a live session limited to
# relay candidates that therefore has to cross the TLS bridge.
#
#   make check-gateway-relay
#
# Needs Docker with coturn/coturn:4.6, openssl, ss, and ports 13478, 13479 and
# 15349 free. See docs/TURN-DEPLOY.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE=coturn/coturn:4.6
NAME=relaysight-gateway-relay-check
PLAIN_PORT=13478
CLOSED_UDP_PORT=13479
TLS_PORT=15349

fail() { echo "FAIL: $*" >&2; exit 1; }

printf '1/4 ports are free... '
if ss -ltnu | grep -qE ":(${PLAIN_PORT}|${CLOSED_UDP_PORT}|${TLS_PORT})\b"; then
  fail "something already listens on ${PLAIN_PORT}, ${CLOSED_UDP_PORT} or ${TLS_PORT}"
fi
echo ok

WORK="$(mktemp -d)"
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

printf '2/4 a throwaway certificate authority and a certificate for localhost... '
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$WORK/ca.key" -out "$WORK/ca.pem" -days 1 -subj /CN=relaysight-check-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$WORK/server.key" -out "$WORK/server.csr" -subj /CN=localhost 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' >"$WORK/server.ext"
openssl x509 -req -in "$WORK/server.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
  -CAcreateserial -out "$WORK/server.pem" -days 1 -extfile "$WORK/server.ext" 2>/dev/null
# coturn runs as nobody inside the container and reads these through the mount.
chmod 0755 "$WORK"
chmod 0644 "$WORK/server.pem" "$WORK/server.key"
echo ok

printf '3/4 coturn listens for TLS on %s... ' "$TLS_PORT"
# --allow-loopback-peers: the fake browser listens on 127.0.0.1, and coturn
# refuses loopback peers by default (--allowed-peer-ip does not lift that).
docker run -d --name "$NAME" --network host -v "$WORK:/certs:ro" "$IMAGE" \
  --listening-ip=127.0.0.1 --listening-port="$PLAIN_PORT" --tls-listening-port="$TLS_PORT" \
  --min-port=49300 --max-port=49340 --external-ip=127.0.0.1 --realm=relay.test \
  --lt-cred-mech --user=check:checkpass --allow-loopback-peers \
  --cert=/certs/server.pem --pkey=/certs/server.key \
  --fingerprint --no-cli --log-file=stdout >/dev/null
for _ in $(seq 1 60); do
  ss -ltn | grep -qE ":${TLS_PORT}\b" && break
  sleep 0.5
done
ss -ltn | grep -qE ":${TLS_PORT}\b" || fail "coturn never listened on ${TLS_PORT}: $(docker logs "$NAME" 2>&1 | tail -5)"
echo ok

echo '4/4 a relay-only live session crosses the TLS bridge:'
export GATEWAY_TURN_CA_FILE="$WORK/ca.pem"
export RELAYSIGHT_TEST_TURNS_URL="turns:localhost:${TLS_PORT}?transport=tcp"
export RELAYSIGHT_TEST_TURN_UDP_URL="turn:127.0.0.1:${CLOSED_UDP_PORT}?transport=udp"
export RELAYSIGHT_TEST_TURN_USER=check
export RELAYSIGHT_TEST_TURN_PASS=checkpass
cd "$ROOT"
cargo test -p vms-gateway -- --ignored --exact \
  live::tests::a_session_relays_over_tls_when_udp_to_the_relay_is_blocked 2>&1 | tee "$WORK/test.log"
grep -q "test result: ok. 1 passed" "$WORK/test.log" \
  || fail "the relayed-session test did not run and pass"

echo "Gateway relay check passed."
