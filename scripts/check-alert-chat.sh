#!/usr/bin/env bash
# Deliver an alert to a real chat service and read it back out of the channel.
#
#   make check-alert-chat
#
# `make check-alert` runs the whole chain to a receiver this repository wrote,
# which proves the plumbing and nothing about chat. This starts Mattermost —
# a chat service somebody else wrote, with its own database, its own incoming
# webhooks and its own API — gives it a team, a channel and a webhook, and
# then raises two alerts: the test one the dashboard button sends, and a real
# camera going offline. Both have to turn up as posts in the channel, with the
# text a person would read on a phone.
#
# It needs Docker. Nothing here is a credential: the webhook belongs to a
# container that is thrown away at the end.
#
# Mattermost rather than Slack because a check nobody can run is not a check:
# a Slack webhook is somebody's private URL, and `make check-alert
# WEBHOOK_URL=…` is still the way to post to one. What this pins down is that
# a real chat service accepts what the sink sends and shows it to a person.
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${MATTERMOST_IMAGE:-mattermost/mattermost-preview:latest}"
CONTAINER=relaysight-chat-check
MM_EMAIL="owner@relaysight.test"
MM_PASSWORD="Relaysight-check-1"
SINK="${WEBHOOK_SINK_PLUGIN:-../relaysight-plugins/webhook-sink/plugin.py}"

command -v docker >/dev/null || { echo "docker is not on PATH; this check needs it." >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "docker is not usable by this user." >&2; exit 1; }
[ -f "$SINK" ] || { cat >&2 <<EOF
the webhook sink plugin is not at $SINK.
Reference plugins live in their own repository; clone it beside this one, or
point WEBHOOK_SINK_PLUGIN at plugin.py.
EOF
exit 1; }

work="$(mktemp -d)"
api_pid=""; sink_pid=""; refresher_pid=""
cleanup() {
    for pid in "$api_pid" "$sink_pid" "$refresher_pid"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
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

mm_port="$(free_port)"
sink_port="$(free_port)"
api_port="$(free_port)"
MM="http://localhost:$mm_port"
API="http://127.0.0.1:$api_port"
PLUGIN_TOKEN="a-plugin-token"

echo "== 1/7 starting Mattermost on $mm_port"
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" -p "$mm_port:8065" "$IMAGE" >/dev/null
for _ in $(seq 1 90); do
    curl -fs -o /dev/null "$MM/api/v4/system/ping" && break
    sleep 2
done
curl -fs -o /dev/null "$MM/api/v4/system/ping" \
    || { echo "Mattermost never came up" >&2; docker logs --tail 30 "$CONTAINER" >&2; exit 1; }

echo "== 2/7 a person, a team and a channel"
# The first account on a fresh Mattermost becomes its system admin, which is
# what lets the rest of this run through its API.
curl -fsS -o /dev/null -X POST -H 'content-type: application/json' \
    -d "$(python3 -c "
import json
print(json.dumps({'email':'$MM_EMAIL','username':'relaysight','password':'$MM_PASSWORD'}))")" \
    "$MM/api/v4/users"
token=$(curl -sS -D- -o /dev/null -X POST -H 'content-type: application/json' \
    -d "$(python3 -c "
import json
print(json.dumps({'login_id':'$MM_EMAIL','password':'$MM_PASSWORD'}))")" \
    "$MM/api/v4/users/login" | grep -i '^token:' | tr -d '\r' | awk '{print $2}')
[ -n "$token" ] || { echo "Mattermost would not log the owner in" >&2; exit 1; }
auth="authorization: Bearer $token"
mm_id() { python3 -c "import sys,json;print(json.load(sys.stdin)['id'])"; }

team=$(curl -fsS -X POST -H "$auth" -H 'content-type: application/json' \
    -d '{"name":"relaysight","display_name":"Relaysight","type":"O"}' \
    "$MM/api/v4/teams" | mm_id)
channel=$(curl -fsS -H "$auth" "$MM/api/v4/teams/$team/channels/name/town-square" | mm_id)

echo "== 3/7 an incoming webhook, from the chat service"
curl -fsS -o /dev/null -X PUT -H "$auth" -H 'content-type: application/json' \
    -d '{"ServiceSettings":{"EnableIncomingWebhooks":true}}' "$MM/api/v4/config/patch"
hook=$(curl -fsS -X POST -H "$auth" -H 'content-type: application/json' \
    -d "$(python3 -c "
import json
print(json.dumps({'channel_id':'$channel','display_name':'Relaysight alerts',
 'description':'alerts raised by the control plane'}))")" \
    "$MM/api/v4/hooks/incoming" | mm_id)
WEBHOOK_URL="$MM/hooks/$hook"

# What the channel holds, newest first, skipping Mattermost's own joined-the-team notices.
posts() {
    curl -sS -H "$auth" "$MM/api/v4/channels/$channel/posts?per_page=50" | python3 -c "
import json, sys
feed = json.load(sys.stdin)
for post_id in feed['order']:
    post = feed['posts'][post_id]
    if post.get('type'):
        continue
    print(post['message'].replace('\n', ' ⏎ '))"
}
# Wait for a post matching a pattern, printing it. Gives up rather than hanging.
wait_for_post() {  # pattern, seconds, what
    for _ in $(seq 1 "$2"); do
        if posts | grep -q -- "$1"; then
            echo "   the channel shows: $(posts | grep -m1 -- "$1")"
            return 0
        fi
        sleep 1
    done
    echo "nothing matching '$1' ever reached the channel ($3)" >&2
    echo "what the channel holds:" >&2
    posts >&2
    return 1
}

echo "== 4/7 the real webhook sink, pointed at it"
PLUGIN_TOKEN="$PLUGIN_TOKEN" PORT="$sink_port" WEBHOOK_URL="$WEBHOOK_URL" \
    WEBHOOK_FORMAT=text MIN_SEVERITY=info \
    python3 "$SINK" > "$work/sink.log" 2>&1 &
sink_pid=$!
sink_health() {
    curl -fs -o /dev/null -H "authorization: Bearer $PLUGIN_TOKEN" \
        "http://127.0.0.1:$sink_port/v1/plugin/health"
}
for _ in $(seq 1 40); do sink_health && break; sleep 0.25; done
sink_health || { echo "the sink never came up" >&2; cat "$work/sink.log" >&2; exit 1; }

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

echo "== 5/7 a control plane that raises real events"
cargo build -q -p vms-api
# The stale window is also the grace period after startup, so an outage
# reported a moment from now is not mistaken for one this process never saw
# begin. Thirty seconds is short enough to wait for and long enough that a
# camera whose gateway is still reporting is not called stale — this check
# wants the other path, the one a site takes when it can say what is wrong.
API_BIND="127.0.0.1:$api_port" \
DATABASE_URL="sqlite:$work/vms.db" \
ADMIN_PASSWORD="a long enough password" \
GATEWAY_TOKEN="a-gateway-token" \
STALE_CAMERA_SECONDS=30 \
PLUGIN_DIR="$work/plugins.d" \
WEBHOOK_PLUGIN_TOKEN="$PLUGIN_TOKEN" \
    ./target/debug/vms-api > "$work/api.log" 2>&1 &
api_pid=$!
for _ in $(seq 1 60); do curl -fs -o /dev/null "$API/healthz" && break; sleep 0.5; done
curl -fs -o /dev/null "$API/healthz" \
    || { echo "the control plane never came up" >&2; cat "$work/api.log" >&2; exit 1; }
cookie=$(curl -sS -D- -o /dev/null -X POST -H 'content-type: application/json' \
    -d '{"email":"admin@localhost","password":"a long enough password"}' \
    "$API/api/v1/auth/login" | grep -i '^set-cookie' | sed 's/.*: //; s/;.*//')

echo "== 6/7 the test alert the dashboard button sends"
answer=$(curl -fsS -X POST -H "cookie: $cookie" "$API/api/v1/events/test")
echo "$answer" | grep -q '"sinks":1' \
    || { echo "the control plane does not see the sink: $answer" >&2; cat "$work/api.log" >&2; exit 1; }
wait_for_post "Test alert" 30 "the test event"

echo "== 7/7 a camera going offline, which is the alert anyone actually wants"
telemetry() {  # status, last_error
    curl -fsS -o /dev/null -X POST -H 'content-type: application/json' \
        -H 'authorization: Bearer a-gateway-token' \
        -d "$(python3 - "$1" "$2" <<'PY'
import datetime, json, sys
status, last_error = sys.argv[1], sys.argv[2]
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
camera = {"camera_id": "cam-yard", "gateway_id": "gw-chat", "site_id": "site-bakery",
          "name": "Yard camera", "status": status, "packet_loss": 0, "reconnects": 0,
          "last_seen": now}
if last_error:
    camera["last_error"] = last_error
print(json.dumps({"gateway_id": "gw-chat", "customer_id": "cust-1",
                  "customer_name": "Customer", "site_id": "site-bakery",
                  "site_name": "Bakery", "city": "Town", "sent_at": now,
                  "cameras": [camera]}))
PY
)" "$API/api/v1/cameras/telemetry"
}
telemetry healthy ""
# Keep reporting it: a gateway that is still talking about a camera it cannot
# reach is an outage with a reason, and the reason is what has to reach the
# channel. A gateway that went quiet instead is a different alert, and the
# control plane would say "telemetry is stale" rather than name the fault.
(
    while :; do
        telemetry offline "no route to host" || true
        sleep 5
    done
) &
refresher_pid=$!
echo "   the gateway now reports the yard camera as offline: no route to host"
# The reconciler runs on the retention cadence, once a minute, so this waits
# for a tick rather than for a knob that exists to make a test quick.
wait_for_post "Yard camera" 150 "the camera going offline"
posts | grep -m1 -- "Yard camera" | grep -q "no route to host" || {
    echo "the alert reached the channel without the reason the gateway gave:" >&2
    posts | grep -m1 -- "Yard camera" >&2
    exit 1
}

echo
version=$(curl -fsS "$MM/api/v4/config/client?format=old" 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin).get('Version','preview'))" 2>/dev/null \
    || echo preview)
echo "A real chat service has received an alert: Mattermost $version accepted what the"
echo "sink sent and showed both in a channel, the camera's own fault among them."
