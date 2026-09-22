#!/usr/bin/env bash
# The gateway installer and its update channel, end to end: a systemd-booted
# Debian container installs the gateway from a local release channel signed
# with a throwaway key, enrols against a fake API, takes a newer release, rolls
# back one that does not come up, refuses one signed by the wrong key, and
# stores a camera password where the running service can read it.
#
#   make check-installer
#
# Needs Docker (the container runs privileged, for systemd), python3, openssl,
# cargo, and ports 18090 and 18091 free. See docs/INSTALL-GATEWAY.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CHANNEL_PORT=18090
API_PORT=18091
CHANNEL="http://127.0.0.1:${CHANNEL_PORT}"
API="http://127.0.0.1:${API_PORT}"
CONTAINER=relaysight-installer-check
IMAGE=relaysight-installer-check:systemd

fail() { echo "FAIL: $*" >&2; exit 1; }
port_taken() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

printf '1/9 ports %s and %s are free... ' "$CHANNEL_PORT" "$API_PORT"
for port in "$CHANNEL_PORT" "$API_PORT"; do
  port_taken "$port" && fail "something already listens on $port"
done
echo ok

WORK="$(mktemp -d)"
PIDS=()
cleanup() {
  if [[ -n "${CHECK_KEEP:-}" ]]; then
    echo "CHECK_KEEP: left $CONTAINER, the servers (${PIDS[*]}) and $WORK in place" >&2
    return
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

in_box() { docker exec "$CONTAINER" "$@"; }
# wait_for SECONDS COMMAND... -- retry once a second until it succeeds or time is up.
wait_for() {
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do
    (( SECONDS < deadline )) || return 1
    sleep 1
  done
}

printf '2/9 building gateways 0.2.0 and 0.3.0 with a throwaway release key... '
"$ROOT/scripts/release-key.sh" generate "$WORK/release.key" >/dev/null
"$ROOT/scripts/release-key.sh" generate "$WORK/impostor.key" >/dev/null
PUBKEY="$("$ROOT/scripts/release-key.sh" public "$WORK/release.key" | sed -n '2p')"
openssl pkey -in "$WORK/release.key" -pubout -out "$WORK/release.pub.pem"
export CARGO_TARGET_DIR="$ROOT/target/installer-check"
for version in 0.2.0 0.3.0; do
  (cd "$ROOT" && GATEWAY_RELEASE_PUBKEY="$PUBKEY" GATEWAY_VERSION="$version" \
    cargo build -q --release -p vms-gateway)
  cp "$CARGO_TARGET_DIR/release/vms-gateway" "$WORK/vms-gateway-$version"
done
"$WORK/vms-gateway-0.3.0" --version | grep -q '0.3.0' || fail "the 0.3.0 build does not say so"
echo ok

publish() { # VERSION BINARY KEY
  rm -rf "$WORK/channel.new"
  "$ROOT/scripts/make-release.sh" "$1" "$WORK/channel.new" "$3" "$CHANNEL" "x86_64=$2" >/dev/null
  cp "$ROOT/deploy/gateway/install.sh" "$WORK/channel.new/"
  rm -rf "$WORK/channel" && mv "$WORK/channel.new" "$WORK/channel"
}

printf '3/9 publishing 0.2.0, and starting the channel and a fake API... '
publish 0.2.0 "$WORK/vms-gateway-0.2.0" "$WORK/release.key"
(cd "$WORK" && exec python3 -m http.server "$CHANNEL_PORT" --bind 127.0.0.1 --directory "$WORK/channel") \
  >/dev/null 2>&1 &
PIDS+=($!)
# The fake API: enrolment burns a token, heartbeats are logged with the version
# they carry, everything else is accepted.
cat >"$WORK/fake_api.py" <<'PY'
import http.server, json, sys
log = sys.argv[2]
class Api(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def reply(self, status, body=b""):
        self.send_response(status)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        self.reply(200, b"null")  # no commands queued
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with open(log, "a") as out:
            if self.path.endswith("/gateways/enroll"):
                out.write("enroll\n")
                self.reply(200, json.dumps({
                    "gateway_token": "enrolled-token",
                    "entitlement": {"edition": "community", "plan": "community",
                                    "self_hosted": True, "managed": False,
                                    "camera_limit": None, "capabilities": []},
                    "customer_id": "cust-1", "customer_name": "Customer",
                    "site_id": "site-1", "site_name": "Site", "city": "Madrid",
                }).encode())
                return
            if self.path.endswith("/gateways/heartbeat"):
                out.write("heartbeat " + json.loads(body)["version"] + "\n")
        self.reply(204)
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Api).serve_forever()
PY
python3 "$WORK/fake_api.py" "$API_PORT" "$WORK/api.log" >/dev/null 2>&1 &
PIDS+=($!)
touch "$WORK/api.log"
wait_for 10 port_taken "$CHANNEL_PORT" || fail "the channel did not start"
wait_for 10 port_taken "$API_PORT" || fail "the fake API did not start"
echo ok

last_version() { grep '^heartbeat ' "$WORK/api.log" | tail -1 | cut -d' ' -f2; }
heartbeats_say() { [[ "$(last_version)" == "$1" ]]; }
# heartbeat_after LINE VERSION -- a heartbeat from VERSION logged after line LINE.
heartbeat_after() {
  tail -n +"$(($1 + 1))" "$WORK/api.log" | grep -q "^heartbeat $2\$"
}
enrolments() { grep -c '^enroll$' "$WORK/api.log" || true; }

printf '4/9 install.sh installs 0.2.0, which enrols and heartbeats... '
docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM debian:trixie
RUN apt-get update && apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus ca-certificates curl openssl passwd \
    && rm -rf /var/lib/apt/lists/*
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
DOCKERFILE
docker run -d --name "$CONTAINER" --privileged --cgroupns=private --network host \
  --tmpfs /run --tmpfs /run/lock "$IMAGE" >/dev/null
# "degraded" is how a container usually boots — some unit that wants real
# hardware fails — and still means systemd is up and running services.
booted() {
  local state
  state="$(in_box systemctl is-system-running 2>/dev/null || true)"
  [[ "$state" == running || "$state" == degraded ]]
}
wait_for 60 booted || fail "systemd did not come up in the container"
docker cp "$WORK/release.pub.pem" "$CONTAINER:/root/release.pub.pem"
in_box sh -c "curl -fsSL $CHANNEL/install.sh | sh -s -- \
    --channel $CHANNEL --pubkey-file /root/release.pub.pem \
    --api-url $API --enrollment-token check-token --gateway-id check-gw \
    --env HEARTBEAT_INTERVAL_SECONDS=2 --env GATEWAY_UPDATE_WAIT_SECONDS=15 \
    --env ONVIF_DISCOVERY_SECONDS=0" >"$WORK/install.log" 2>&1 \
  || fail "install.sh failed: $(tail -5 "$WORK/install.log")"
in_box systemctl is-active --quiet relaysight-gateway || fail "the service is not running"
wait_for 30 heartbeats_say 0.2.0 || fail "no heartbeat from 0.2.0 (last: '$(last_version)')"
[[ "$(enrolments)" == 1 ]] || fail "expected one enrolment, saw $(enrolments)"
in_box systemctl is-enabled --quiet relaysight-gateway-update.timer || fail "no update timer"
echo ok

run_update() { in_box systemctl start relaysight-gateway-update.service 2>/dev/null; }
update_log() { in_box journalctl -u relaysight-gateway-update.service --no-pager -n 12 -o cat; }

printf '5/9 the update takes 0.3.0, keeping its identity... '
publish 0.3.0 "$WORK/vms-gateway-0.3.0" "$WORK/release.key"
run_update || fail "the update service failed on a good release:
$(update_log)"
wait_for 30 heartbeats_say 0.3.0 || fail "no heartbeat from 0.3.0 (last: '$(last_version)')"
[[ "$(in_box /usr/local/bin/vms-gateway --version)" == "vms-gateway 0.3.0" ]] || fail "the binary is not 0.3.0"
[[ "$(enrolments)" == 1 ]] || fail "the update enrolled again: its identity did not survive"
echo ok

printf '6/9 a release that does not come up is rolled back... '
printf '#!/bin/sh\nexit 1\n' >"$WORK/broken"
publish 0.4.0 "$WORK/broken" "$WORK/release.key"
if run_update; then fail "the update service reported success for a binary that exits at once"; fi
[[ "$(in_box /usr/local/bin/vms-gateway --version)" == "vms-gateway 0.3.0" ]] || fail "0.3.0 is not back in place"
mark="$(wc -l <"$WORK/api.log")"
wait_for 30 heartbeat_after "$mark" 0.3.0 || fail "0.3.0 is not heartbeating after the rollback"
echo ok

printf '7/9 a release signed with another key is refused... '
publish 0.5.0 "$WORK/vms-gateway-0.3.0" "$WORK/impostor.key"
if run_update; then fail "the update service accepted a release signed with the wrong key"; fi
in_box journalctl -u relaysight-gateway-update.service --no-pager -n 20 \
  | grep -q 'not signed by the release key' || fail "the refusal did not say why"
[[ "$(in_box /usr/local/bin/vms-gateway --version)" == "vms-gateway 0.3.0" ]] || fail "the binary changed"
echo ok

printf '8/9 the credentials wrapper writes where the service reads... '
in_box sh -c 'printf "%s\n" camera-secret | relaysight-gateway-credentials set 10.0.0.9 admin' >/dev/null \
  || fail "the credentials wrapper failed"
in_box relaysight-gateway-credentials list | grep -q '10.0.0.9' || fail "the entry is not listed"
[[ "$(in_box stat -c %U /var/lib/relaysight-gateway/camera-credentials.enc)" == relaysight-gateway ]] \
  || fail "the credential file is not the service user's"
in_box systemctl restart relaysight-gateway
wait_for 30 in_box systemctl is-active --quiet relaysight-gateway \
  || fail "the service does not start with the credential file in place"
echo ok

printf '9/9 re-running install.sh leaves a working install alone... '
publish 0.3.0 "$WORK/vms-gateway-0.3.0" "$WORK/release.key"
in_box sh -c "curl -fsSL $CHANNEL/install.sh | sh -s -- --channel $CHANNEL --pubkey-file /root/release.pub.pem" \
  >"$WORK/reinstall.log" 2>&1 || fail "re-running install.sh failed: $(tail -5 "$WORK/reinstall.log")"
in_box grep -q "^API_URL='$API'" /etc/relaysight/gateway.env || fail "the re-run lost the configuration"
[[ "$(enrolments)" == 1 ]] || fail "the re-run enrolled again"
in_box systemctl is-active --quiet relaysight-gateway || fail "the service is not running after the re-run"
echo ok

echo "Installer check passed."
