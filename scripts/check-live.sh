#!/usr/bin/env bash
# Watch live video arrive in a real browser, and fail if no frame is decoded.
#
#   make check-live
#
# Everything else in the live path is checked against doubles: unit tests, a
# fake browser that offers what webrtc-rs registers, and a real Chrome offer
# kept as a fixture. None of them can tell whether a browser ends up with
# pixels — the fake browser agrees with the gateway by construction, and the
# first time a real one was pointed at this it found two bugs in a day.
#
# So this runs the whole chain as separate processes: ffmpeg publishing the
# committed H.264 fixture the way an encoder does, the real gateway, the real
# control plane, and a headless Firefox that logs in, asks for live video,
# answers the offer and reports `framesDecoded` off `getStats()`.
#
# What it does not cover: the RTSP dial. ffmpeg can receive RTSP but not serve
# it, and a fake RTSP camera is a bigger thing than this check; the dial has
# unit tests and a real NVR behind it (docs/DEVICE-MATRIX.md). This check is
# about the half no double can reach — the browser.
#
# No credentials are committed: the admin password and the gateway token are
# generated per run and live only in the temporary directory.
set -euo pipefail

cd "$(dirname "$0")/.."

need() {
    command -v "$1" >/dev/null || { echo "$1 is not on PATH; $2" >&2; exit 1; }
}
need ffmpeg "it stands in for the encoder here"
need firefox "this check is about a real browser, so there is nothing to fall back to"

# Firefox ships OpenH264 and leaves it off, and a fresh profile has not
# downloaded it at all. Borrow the one an installed profile already has:
# without H.264 the browser cannot receive this stream and the check would
# fail for a reason that is not the gateway's.
plugin="$(find "$HOME/.mozilla/firefox" -maxdepth 3 -type d -name '2*' -path '*gmp-gmpopenh264*' 2>/dev/null | sort | tail -1)"
[ -n "$plugin" ] || { cat >&2 <<'EOF'
no OpenH264 plugin found under ~/.mozilla/firefox.
Open Firefox, set media.gmp-gmpopenh264.enabled to true in about:config and
let it download the plugin (about:support lists it under Media), then run this
again. Firefox 140 ships it disabled, which is its own finding — docs/LIVE.md.
EOF
exit 1; }

work="$(mktemp -d)"
api_pid=""; gw_pid=""; ff_pid=""; web_pid=""; wall_web_pid=""
cleanup() {
    # The publisher is a pipeline in its own session: kill the group, or cat
    # and ffmpeg outlive the check.
    [ -n "$ff_pid" ] && kill -- "-$ff_pid" 2>/dev/null || true
    for pid in "$wall_web_pid" "$web_pid" "$gw_pid" "$api_pid"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

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
        sleep 1
    done
    return 1
}

API_PORT="$(free_port)"; RTMP_PORT="$(free_port)"; WEB_PORT="$(free_port)"; WALL_PORT="$(free_port)"
ADMIN_PASSWORD="check-live-$(head -c 9 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9')"
GATEWAY_TOKEN="check-live-token-$(head -c 6 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9')"
STREAM_KEY="checklive"
API="http://127.0.0.1:$API_PORT"

echo "1/9 building the control plane and the gateway"
cargo build -q -p vms-api -p vms-gateway

echo "2/9 starting the control plane on $API_PORT"
# No STUN. The browser and the gateway are on this one machine, so host
# candidates are the path, and a STUN server would turn a local check into a
# question about this host's interface list and its route to the internet.
# That used to cost a session twelve seconds here: a socket on a Docker bridge
# is never answered, and the webrtc-rs pinned then left it gathering for ever
# (relaysight-vms-0kem). It does not any more — the gateway answers as soon as
# it holds one candidate of every kind its servers can produce, and 0.21.1
# gives such a transaction up after about eleven seconds rather than never
# (relaysight-vms-9vlo). Running this check with Google's STUN restored is how
# both were proved, and the answer came 41ms after the offer. What a server
# would still buy is four reflexive candidates nothing on a loopback path
# needs, and a check that fails when this machine's route to the internet
# does.
API_BIND="127.0.0.1:$API_PORT" \
DATABASE_URL="sqlite:$work/vms.db" \
ADMIN_PASSWORD="$ADMIN_PASSWORD" \
GATEWAY_TOKEN="$GATEWAY_TOKEN" \
RTC_STUN_URLS="" \
RUST_LOG=warn \
    ./target/debug/vms-api >"$work/api.log" 2>&1 &
api_pid=$!
wait_for "curl -fs -o /dev/null $API/healthz" || { echo "the control plane did not come up" >&2; tail -n 5 "$work/api.log" >&2; exit 1; }

echo "3/9 starting the gateway, RTMP ingest on $RTMP_PORT"
API_URL="$API" \
GATEWAY_ID=check-live \
SITE_ID=check-live \
GATEWAY_TOKEN="$GATEWAY_TOKEN" \
GATEWAY_STATE_DIR="$work/gateway" \
RTMP_LISTEN="127.0.0.1:$RTMP_PORT" \
ONVIF_DISCOVERY_SECONDS=0 \
CAMERA_PROBE_INTERVAL_SECONDS=5 \
RUST_LOG=warn \
    ./target/debug/vms-gateway >"$work/gateway.log" 2>&1 &
gw_pid=$!

cookie="$work/cookie"
login() {
    curl -fs -c "$cookie" -H 'content-type: application/json' \
        -d "{\"email\":\"admin@localhost\",\"password\":\"$ADMIN_PASSWORD\"}" \
        -o /dev/null "$API/api/v1/auth/login"
}
wait_for login || { echo "could not log in as the seeded owner" >&2; exit 1; }
wait_for "curl -fs -b $cookie $API/api/v1/gateways | grep -q check-live" \
    || { echo "the gateway never reported in" >&2; tail -n 5 "$work/gateway.log" >&2; exit 1; }

echo "4/9 registering the pushed source and publishing the fixture"
curl -fs -b "$cookie" -H 'content-type: application/json' \
    -d "{\"gateway_id\":\"check-live\",\"name\":\"Check live\",\"kind\":\"rtmp\",\"address\":\"$STREAM_KEY\"}" \
    -o /dev/null "$API/api/v1/sources"

# The gateway refuses a publisher on a key it has not been told about, and
# ffmpeg does not retry, so wait until the source has reached the gateway —
# it reports the key as a camera with nothing publishing to it.
wait_for "curl -fs -b $cookie $API/api/v1/cameras | grep -q 'Check live'" 60 \
    || { echo "the gateway never picked up the pushed source" >&2; tail -n 5 "$work/gateway.log" >&2; exit 1; }

# The fixture is three and a half seconds long and raw H.264 cannot be
# seeked, so `-stream_loop` fails on the second pass with "Operation not
# permitted" and the publisher quietly stops. Feeding it through a pipe needs
# no seeking, and the stream never ends. setsid so the whole pipeline can be
# killed as one group.
setsid bash -c "while :; do cat edge/gateway/fixtures/camera.h264; done \
    | ffmpeg -hide_banner -loglevel error -re -f h264 -framerate 25 -i pipe:0 -c copy \
      -f flv 'rtmp://127.0.0.1:$RTMP_PORT/live/$STREAM_KEY'" \
    >"$work/ffmpeg.log" 2>&1 &
ff_pid=$!

camera_id() {
    curl -fs -b "$cookie" "$API/api/v1/cameras" | python3 -c "
import json, sys
for camera in json.load(sys.stdin):
    if camera.get('status') == 'healthy':
        print(camera['camera_id'])
        break
"
}
wait_for "test -n \"\$(camera_id)\"" 90 || {
    echo "the pushed stream never became a healthy camera" >&2
    curl -fs -b "$cookie" "$API/api/v1/cameras" >&2 || true
    echo >&2
    echo "--- ffmpeg:" >&2; head -n 12 "$work/ffmpeg.log" >&2
    echo "--- gateway:" >&2; grep -iE "rtmp|publish|ingest" "$work/gateway.log" | head -n 8 >&2
    exit 1
}
echo "    publishing, camera is healthy"

echo "5/9 serving the dashboard origin on $WEB_PORT"
# The browser needs one origin for the page and the API: a session cookie is
# origin-scoped, and the real deployment puts nginx in front of both.
cat > "$work/serve.py" <<PY
import http.server, sys, urllib.error, urllib.request
API = "$API"
ROOT, PORT = sys.argv[1], int(sys.argv[2])
class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=ROOT, **k)
    def _proxy(self, body=None):
        request = urllib.request.Request(API + self.path, data=body, method=self.command)
        for header in ("content-type", "cookie"):
            if self.headers.get(header):
                request.add_header(header, self.headers[header])
        try:
            with urllib.request.urlopen(request) as upstream:
                self.send_response(upstream.status)
                for key, value in upstream.getheaders():
                    if key.lower() not in ("transfer-encoding", "connection"):
                        self.send_header(key, value)
                self.end_headers()
                self.wfile.write(upstream.read())
        except urllib.error.HTTPError as err:
            self.send_response(err.code)
            for key, value in err.headers.items():
                if key.lower() not in ("transfer-encoding", "connection"):
                    self.send_header(key, value)
            self.end_headers()
            self.wfile.write(err.read() or b"")
    def do_GET(self):
        self._proxy() if self.path.startswith("/api/") else super().do_GET()
    def do_POST(self):
        self._proxy(self.rfile.read(int(self.headers.get("content-length") or 0)))
    def log_message(self, *a):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
PY
mkdir -p "$work/web"
sed -e "s|@PASSWORD@|$ADMIN_PASSWORD|" scripts/live-check-page.html > "$work/web/index.html"
python3 "$work/serve.py" "$work/web" "$WEB_PORT" >"$work/web.log" 2>&1 &
web_pid=$!
wait_for "curl -fs -o /dev/null http://127.0.0.1:$WEB_PORT/" || { echo "the page did not come up" >&2; exit 1; }

echo "6/9 driving a real Firefox at it"
profile="$work/profile"
mkdir -p "$profile"
cp -r "$(dirname "$plugin")" "$profile/"
plugin_version="$(basename "$plugin")"
cat > "$profile/prefs.js" <<PY
user_pref("browser.dom.window.dump.enabled", true);
user_pref("media.gmp-gmpopenh264.enabled", true);
user_pref("media.gmp-gmpopenh264.version", "$plugin_version");
user_pref("media.gmp-gmpopenh264.abi", "x86_64-gcc3");
user_pref("media.peerconnection.video.h264_enabled", true);
// Real host addresses rather than <uuid>.local. Firefox hides them behind mDNS
// by default, and the gateway then spends ten seconds asking a responder that
// may not be there — on this machine it is not (docs/LIVE.md). The check is
// about pixels, not about who holds port 5353.
user_pref("media.peerconnection.ice.obfuscate_host_addresses", false);
PY
timeout 180 firefox --headless --profile "$profile" --no-remote \
    "http://127.0.0.1:$WEB_PORT/" 2>&1 | grep --line-buffered '^CHECKLIVE' | tee "$work/browser.log" || true

echo "7/9 what the browser got"
python3 - "$work/browser.log" <<'PY'
import re, sys
lines = open(sys.argv[1]).read().splitlines()
for line in lines:
    print("   ", line.removeprefix("CHECKLIVE ").strip())
decoded = 0
for line in lines:
    found = re.search(r"decoded=(\d+)", line)
    if found:
        decoded = max(decoded, int(found.group(1)))
if decoded > 0:
    print(f"\nlive video reached a real browser: {decoded} frames decoded")
else:
    print("\nno frame was decoded: the browser got a session and no pixels", file=sys.stderr)
    sys.exit(1)
PY

echo "8/9 opening the real dashboard, and its wall, in the same browser"
# The page above stands in for a browser; this is the shipped dashboard —
# app.html, dashboard-app.js, wall.js and the stylesheet, out of web/ — with
# one module injected ahead of its own, to log in and to keep hold of the peer
# connections the wall opens. Everything else on the page is what ships.
mkdir -p "$work/wall"
cp -r web/. "$work/wall/"
rm -rf "$work/wall/node_modules" "$work/wall/tests" "$work/wall/package.json" "$work/wall/package-lock.json"
sed -e "s|@PASSWORD@|$ADMIN_PASSWORD|" scripts/wall-check-page.js > "$work/wall/wall-check.js"
python3 - "$work/wall/app.html" <<'ZPYZ'
import io, sys
path = sys.argv[1]
html = io.open(path, encoding="utf-8").read()
anchor = '<script type="module" src="dashboard.js"></script>'
assert anchor in html, "app.html no longer loads dashboard.js; the wall check cannot inject its driver"
io.open(path, "w", encoding="utf-8").write(
    html.replace(anchor, '<script type="module" src="wall-check.js"></script>\n  ' + anchor, 1))
ZPYZ
cp "$work/wall/app.html" "$work/wall/index.html"
python3 "$work/serve.py" "$work/wall" "$WALL_PORT" >"$work/wall-web.log" 2>&1 &
wall_web_pid=$!
wait_for "curl -fs -o /dev/null http://127.0.0.1:$WALL_PORT/" || { echo "the dashboard did not come up" >&2; cat "$work/wall-web.log" >&2; exit 1; }
# Headless Firefox ignores the page's own window.close(), so each browser step
# costs its whole timeout however quickly the driver finishes — the lines below
# arrive about twenty seconds in. Worth shortening by killing the browser on
# the `done` line, but not at the price of a check that cuts a slow run short.
timeout 240 firefox --headless --profile "$profile" --no-remote \
    "http://127.0.0.1:$WALL_PORT/" 2>&1 | grep --line-buffered '^CHECKWALL' | tee "$work/wall.log" || true

echo "9/9 what the wall did"
python3 - "$work/wall.log" <<'ZPYZ'
import re, sys

lines = open(sys.argv[1]).read().splitlines()
for line in lines:
    print("   ", line.removeprefix("CHECKWALL ").strip())
text = "\n".join(lines)


def number(pattern):
    # The largest, not the first: the driver reports decoded=0 while it waits,
    # and reading the first match would call a working wall a failure.
    found = [int(value) for value in re.findall(pattern, text)]
    return max(found) if found else 0


tiles = number(r"tiles=(\d+)")
decoded = number(r"decoded=(\d+)")
width = number(r"tile=(\d+)x\d+")
height = number(r"tile=\d+x(\d+)")
columns = re.search(r"columns=(.+)", text)
shown = columns.group(1).strip() if columns else "-"
tracks = len(shown.split()) if columns else 0

problems = []
if tiles < 1:
    problems.append("the wall rendered no tiles")
# A grid that collapsed, or tiles the size of a line of text, pass every unit
# test there is: jsdom has no layout engine at all.
if width < 200 or height < 100:
    problems.append(f"a tile is {width}x{height}, which is not a picture anybody can watch")
if tracks != 2:
    problems.append(f"a two-column layout produced {tracks} column(s): {shown}")
if decoded < 1:
    problems.append("no frame was decoded in a tile: the wall got a session and no pixels")

if problems:
    print()
    for problem in problems:
        print(f"FAIL {problem}", file=sys.stderr)
    sys.exit(1)
print(f"\nthe wall carried live video in a real browser: {decoded} frames decoded in a {width}x{height} tile")
ZPYZ
