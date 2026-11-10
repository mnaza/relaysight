#!/usr/bin/env bash
# Raise an alert and watch it arrive somewhere a person would see it.
#
#   make check-alert                       # to a receiver on this machine
#   make check-alert WEBHOOK_URL=https://hooks.slack.com/services/...
#   WEBHOOK_FORMAT=discord make check-alert    # the shape Discord asks for
#
# Everything in the alerts path is tested against doubles: the core against a
# sink in the test process, the sink against its own decisions. This runs the
# real chain — the control plane raising a real event, its outbox, the real
# webhook-sink plugin over HTTP, and a URL. With WEBHOOK_URL set to a chat
# service, the message lands in the channel and a person sees it.
#
# The URL is a credential. It is passed in, never committed, and this script
# does not print it.
set -euo pipefail

cd "$(dirname "$0")/.."

SINK="${WEBHOOK_SINK_PLUGIN:-../relaysight-plugins/webhook-sink/plugin.py}"
[ -f "$SINK" ] || { cat >&2 <<EOF
the webhook sink plugin is not at $SINK.
Reference plugins live in their own repository; clone it beside this one, or
point WEBHOOK_SINK_PLUGIN at plugin.py.
EOF
exit 1; }

work="$(mktemp -d)"
api_pid=""; sink_pid=""; receiver_pid=""
cleanup() {
    for pid in "$api_pid" "$sink_pid" "$receiver_pid"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
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

target="${WEBHOOK_URL:-}"
if [ -z "$target" ]; then
    # Nothing real to post to, so stand something up that at least writes down
    # what arrived. This proves the chain; it does not prove a chat service
    # has ever received one, and the last line says so.
    receiver_port="$(free_port)"
    python3 - "$receiver_port" "$work/received.json" > "$work/receiver.log" 2>&1 <<'PY' &
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port, out = int(sys.argv[1]), sys.argv[2]


class Receiver(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with open(out, "wb") as handle:
            handle.write(body)
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *_args):
        pass


ThreadingHTTPServer(("127.0.0.1", port), Receiver).serve_forever()
PY
    receiver_pid=$!
    target="http://127.0.0.1:$receiver_port/hook"
    real="no"
else
    real="yes"
fi

PLUGIN_TOKEN="a-plugin-token"
sink_port="$(free_port)"
api_port="$(free_port)"

PLUGIN_TOKEN="$PLUGIN_TOKEN" PORT="$sink_port" WEBHOOK_URL="$target" \
    WEBHOOK_FORMAT="${WEBHOOK_FORMAT:-text}" MIN_SEVERITY=info \
    python3 "$SINK" > "$work/sink.log" 2>&1 &
sink_pid=$!
sink_health() {
    curl -fs -o /dev/null -H "authorization: Bearer $PLUGIN_TOKEN" \
        "http://127.0.0.1:$sink_port/v1/plugin/health"
}
for _ in $(seq 1 40); do sink_health && break; sleep 0.25; done
sink_health || { echo "the sink never came up" >&2; cat "$work/sink.log" >&2; exit 1; }
if [ "$real" = "yes" ]; then
    echo "== the webhook sink is running, posting to the URL you supplied"
else
    echo "== the webhook sink is running, posting to a receiver on this machine"
fi

mkdir -p "$work/plugins.d"
python3 - "$work/plugins.d/webhook-sink.json" "$sink_port" <<'PY'
import json, sys
path, port = sys.argv[1], sys.argv[2]
json.dump({
    "endpoint": f"http://127.0.0.1:{port}",
    "enabled": True,
    "token_env": "WEBHOOK_PLUGIN_TOKEN",
    "manifest": {
        "id": "webhook-sink", "name": "Webhook Event Sink", "version": "0.1.0",
        "protocol_version": 1, "vendor": "example",
        "description": "Post fleet events to a webhook.",
        "capabilities": ["event_sink"],
    },
    "placement": "control_plane",
}, open(path, "w"), indent=2)
PY

cargo build -q -p vms-api
API_BIND="127.0.0.1:$api_port" \
DATABASE_URL="sqlite:$work/vms.db" \
ADMIN_PASSWORD="a long enough password" \
PLUGIN_DIR="$work/plugins.d" \
WEBHOOK_PLUGIN_TOKEN="$PLUGIN_TOKEN" \
    ./target/debug/vms-api > "$work/api.log" 2>&1 &
api_pid=$!
API="http://127.0.0.1:$api_port"
for _ in $(seq 1 60); do curl -fs -o /dev/null "$API/healthz" && break; sleep 0.5; done

cookie=$(curl -sS -D- -o /dev/null -X POST -H 'content-type: application/json' \
    -d '{"email":"admin@localhost","password":"a long enough password"}' \
    "$API/api/v1/auth/login" | grep -i '^set-cookie' | sed 's/.*: //; s/;.*//')

echo "== raising an alert"
answer=$(curl -fsS -X POST -H "cookie: $cookie" "$API/api/v1/events/test")
echo "$answer" | grep -q '"sinks":1' \
    || { echo "the control plane does not see the sink: $answer" >&2; cat "$work/api.log" >&2; exit 1; }

echo "== where it got to"
delivered() {
    curl -sS -H "cookie: $cookie" "$API/api/v1/events" | python3 -c "
import json, sys
for event in json.load(sys.stdin):
    for delivery in event.get('deliveries', []):
        if delivery['plugin_id'] == 'webhook-sink' and delivery['delivered_at'] and not delivery['declined']:
            print('yes')
            raise SystemExit
print('no')"
}
for _ in $(seq 1 20); do [ "$(delivered)" = "yes" ] && break; sleep 0.5; done
[ "$(delivered)" = "yes" ] || {
    echo "the alert never reached the sink:" >&2
    curl -sS -H "cookie: $cookie" "$API/api/v1/events" >&2
    cat "$work/sink.log" >&2
    exit 1
}
echo "   the control plane records it as delivered"

if [ "$real" = "no" ]; then
    [ -s "$work/received.json" ] || { echo "the receiver was never posted to" >&2; exit 1; }
    echo "   and the receiver got: $(head -c 200 "$work/received.json")"
    # Which key the body carries is the whole of a chat service's contract:
    # Slack and Mattermost read "text", Discord reads "content" and ignores
    # anything else, so a service handed the wrong one answers 400 and the
    # alert is lost. Discord was handed the wrong one until
    # relaysight-vms-bwir, so the check reads the key rather than trusting
    # that something was delivered.
    python3 - "$work/received.json" "${WEBHOOK_FORMAT:-text}" <<'SHAPE'
import json
import sys

body = json.load(open(sys.argv[1]))
fmt = sys.argv[2]
want = {"text": "text", "discord": "content"}.get(fmt)
if want is None:
    # The json format posts the event itself, for an endpoint of your own.
    if "kind" not in body:
        sys.exit(f"the {fmt} format should have sent the event: {body}")
elif not body.get(want):
    sys.exit(
        f"the {fmt} format has to post the key that service reads, "
        f'"{want}", and the receiver got: {body}'
    )
SHAPE
    echo
    echo "The chain works end to end. Nothing real was posted to — pass"
    echo "WEBHOOK_URL=<your Slack, Discord or Telegram webhook> to do that."
else
    echo
    echo "Posted to the URL you supplied. Go and look at the channel: an alert"
    echo "titled \"Test alert from the dashboard\" should be sitting in it."
fi
