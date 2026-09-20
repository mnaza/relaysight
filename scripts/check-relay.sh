#!/usr/bin/env bash
# The relay's TLS side, checked against the real compose file with throwaway
# certificates: the certificate hook, TURN over TLS on 443, an authenticated
# allocation, renewal without a restart, and a healthcheck that notices when
# coturn quietly starts without TLS.
#
#   make check-relay
#
# Needs Docker, openssl, ss, and ports 443 and 3478 free. See docs/TURN-DEPLOY.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE=(docker compose -f "$ROOT/deploy/coturn/docker-compose.yml")
IMAGE=coturn/coturn:4.6

fail() { echo "FAIL: $*" >&2; exit 1; }

printf '1/12 ports 443 and 3478 are free... '
# First, and before the cleanup trap exists: that trap stops the coturn compose
# project, which would be somebody's real relay if one is running here.
if ss -ltn | grep -qE ':(443|3478)\b'; then
  fail "something already listens on 443 or 3478"
fi
echo ok

WORK="$(mktemp -d)"
RTC_TURN_SECRET="$(openssl rand -hex 16)"
RTC_TURN_CERT_GROUP="$(id -g)"
export RTC_TURN_REALM=relay.test RTC_TURN_PUBLIC_IP=127.0.0.1 RTC_TURN_SECRET RTC_TURN_CERT_GROUP
export RTC_TURN_CERT_DIR="$WORK/certs"
cleanup() {
  "${COMPOSE[@]}" down --timeout 2 >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# A self-signed pair, laid out the way certbot lays out a lineage.
make_cert() {
  mkdir -p "$1"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout "$1/privkey.pem" -out "$1/fullchain.pem" -days 1 -subj "/CN=${2:-relay.test}" 2>/dev/null
}
# Dating a certificate in the past needs openssl 3.5 or newer; older ones can
# only count days forward, so the step that uses this skips itself instead.
make_expired_cert() {
  mkdir -p "$1"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout "$1/privkey.pem" -out "$1/fullchain.pem" \
    -not_before 20250101000000Z -not_after 20250102000000Z \
    -subj /CN=relay.test 2>/dev/null
}
install_cert() {
  RENEWED_LINEAGE="$1" CERT_DIR="$RTC_TURN_CERT_DIR" CERT_GROUP="$RTC_TURN_CERT_GROUP" \
    "$ROOT/deploy/coturn/install-cert.sh" 2>&1
}
serial_of() { openssl x509 -in "$1" -noout -serial; }
served_serial() {
  timeout 5 openssl s_client -connect 127.0.0.1:443 -servername relay.test </dev/null 2>/dev/null \
    | openssl x509 -noout -serial 2>/dev/null || true
}
presents() { [[ "$(served_serial)" == "$1" ]]; }
container() { "${COMPOSE[@]}" ps -q coturn; }
health() {
  docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$(container)" 2>/dev/null \
    || echo gone
}
health_is() { [[ "$(health)" == "$1" ]]; }
# wait_for SECONDS COMMAND... -- retry once a second until it succeeds or time is up.
wait_for() {
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do
    (( SECONDS < deadline )) || return 1
    sleep 1
  done
}
uclient() {
  timeout 30 docker run --rm --network host --entrypoint turnutils_uclient "$IMAGE" \
    -S -t -p 443 -W "$1" -u check -n 1 -m 1 -y 127.0.0.1 2>&1 || true
}

printf '2/12 install-cert.sh installs certificate A before the relay runs... '
make_cert "$WORK/a"
SERIAL_A="$(serial_of "$WORK/a/fullchain.pem")"
out="$(install_cert "$WORK/a")" || fail "install-cert.sh refused certificate A: $out"
grep -q 'coturn is not running' <<<"$out" || fail "install-cert.sh did not notice the relay is down: $out"
echo ok

printf '3/12 443 presents certificate A... '
"${COMPOSE[@]}" up -d >/dev/null
wait_for 30 presents "$SERIAL_A" || fail "443 does not present certificate A (got '$(served_serial)')"
echo ok

printf '4/12 the healthcheck passes... '
wait_for 60 health_is healthy || fail "relay health is '$(health)', expected healthy"
echo ok

printf '5/12 an allocation over TLS authenticates with the shared secret... '
out="$(uclient "$RTC_TURN_SECRET")"
# coturn checks the peer address only inside an authenticated allocation, and
# the shipped blacklist forbids loopback peers: this 403 means we got that far.
grep -q 'error 403 (Forbidden IP)' <<<"$out" || fail "no authenticated allocation over TLS: $out"
! grep -q 'Cannot complete Allocation' <<<"$out" || fail "allocation failed with the shared secret: $out"
echo ok

printf '6/12 a wrong secret cannot allocate over TLS... '
out="$(uclient "wrong-$RTC_TURN_SECRET")"
grep -q 'Cannot complete Allocation' <<<"$out" || fail "a wrong secret was not refused: $out"
echo ok

printf '7/12 install-cert.sh refuses a key that does not belong to its certificate... '
make_cert "$WORK/b"
make_cert "$WORK/mismatch"
cp "$WORK/b/privkey.pem" "$WORK/mismatch/privkey.pem"
if out="$(install_cert "$WORK/mismatch")"; then
  fail "install-cert.sh accepted a mismatched pair: $out"
fi
[[ "$(serial_of "$RTC_TURN_CERT_DIR/fullchain.pem")" == "$SERIAL_A" ]] \
  || fail "a refused install replaced the installed certificate"
cmp -s "$RTC_TURN_CERT_DIR/privkey.pem" "$WORK/a/privkey.pem" \
  || fail "a refused install replaced the installed key"
echo ok

printf '8/12 certificate B replaces A without a restart... '
SERIAL_B="$(serial_of "$WORK/b/fullchain.pem")"
started="$(docker inspect -f '{{.State.StartedAt}}' "$(container)")"
out="$(install_cert "$WORK/b")" || fail "install-cert.sh refused certificate B: $out"
grep -q 'signalled coturn to reload' <<<"$out" || fail "install-cert.sh did not signal the running relay: $out"
wait_for 10 presents "$SERIAL_B" || fail "443 still presents '$(served_serial)' after installing B"
[[ "$(docker inspect -f '{{.State.StartedAt}}' "$(container)")" == "$started" ]] \
  || fail "the relay restarted to pick up B"
echo ok

printf '9/12 a certificate for another name turns the relay unhealthy... '
make_cert "$WORK/wrong-name" other.test
install_cert "$WORK/wrong-name" >/dev/null || fail "install-cert.sh refused the wrong-name certificate"
wait_for 90 health_is unhealthy \
  || fail "relay health is '$(health)' while serving a certificate for another name"
install_cert "$WORK/b" >/dev/null || fail "install-cert.sh refused certificate B on the way back"
wait_for 90 health_is healthy || fail "relay health is '$(health)' after the right certificate came back"
echo ok

printf '10/12 an expired certificate turns the relay unhealthy... '
if make_expired_cert "$WORK/expired"; then
  install_cert "$WORK/expired" >/dev/null || fail "install-cert.sh refused the expired certificate"
  wait_for 90 health_is unhealthy \
    || fail "relay health is '$(health)' while serving an expired certificate"
  echo ok
else
  echo "skipped (openssl here cannot date a certificate in the past)"
fi

printf '11/12 an unreadable key turns the relay unhealthy... '
install_cert "$WORK/b" >/dev/null || fail "install-cert.sh refused certificate B"
chmod 0600 "$RTC_TURN_CERT_DIR/privkey.pem"
"${COMPOSE[@]}" restart coturn >/dev/null
wait_for 90 health_is unhealthy || fail "relay health is '$(health)' with an unreadable key, expected unhealthy"
echo ok

printf '12/12 install-cert.sh says so when the TLS listener never came up... '
# coturn creates its TLS listeners at start or not at all: started without a
# readable certificate, it stays closed on 443 and a reload signal changes
# nothing. The hook has to say that rather than report a reload.
"${COMPOSE[@]}" down --timeout 2 >/dev/null 2>&1 || true
rm -f "$RTC_TURN_CERT_DIR/fullchain.pem" "$RTC_TURN_CERT_DIR/privkey.pem"
"${COMPOSE[@]}" up -d >/dev/null
sleep 3
if out="$(install_cert "$WORK/b")"; then
  fail "install-cert.sh reported success with 443 closed: $out"
fi
grep -qi 'restart' <<<"$out" || fail "install-cert.sh did not say to restart the relay: $out"
[[ "$(serial_of "$RTC_TURN_CERT_DIR/fullchain.pem")" == "$SERIAL_B" ]] \
  || fail "the certificate was not installed before the hook complained"
"${COMPOSE[@]}" restart coturn >/dev/null
wait_for 90 health_is healthy || fail "relay health is '$(health)' after the restart that opens 443"
echo ok

echo "Relay check passed."
