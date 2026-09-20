#!/usr/bin/env bash
# Seed a fleet into a running stack, without a camera or a gateway container:
# log in, create an enrollment, enroll a gateway over HTTP, and post one
# telemetry batch. Then say how many cameras the API kept — which is the whole
# point when the stack runs on a capped plan.
#
#   make demo-fleet                      # five cameras into localhost:8080
#   CAMERAS=9 API_URL=http://host:8080 scripts/demo-fleet.sh
set -euo pipefail

API_URL="${API_URL:-http://localhost:8080}"
ADMIN_PASSWORD="${ADMIN_PASSWORD:-demo-admin-password}"
CAMERAS="${CAMERAS:-5}"
CUSTOMER_ID="${CUSTOMER_ID:-pilot-customer}"
CUSTOMER_NAME="${CUSTOMER_NAME:-Pilot customer}"
SITE_ID="${SITE_ID:-madrid-demo}"
SITE_NAME="${SITE_NAME:-Madrid demo}"
SITE_CITY="${SITE_CITY:-Madrid}"
GATEWAY_ID="${GATEWAY_ID:-demo-gateway-$(date +%s)}"

json() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }

cookie_jar="$(mktemp)"
trap 'rm -f "$cookie_jar"' EXIT

printf 'log in... '
curl -fsS -c "$cookie_jar" -X POST "${API_URL}/api/v1/auth/login" \
  -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,os; print(json.dumps({"password": os.environ["ADMIN_PASSWORD"]}))')" \
  >/dev/null
echo ok

printf 'create enrollment... '
enrollment="$(curl -fsS -b "$cookie_jar" -X POST "${API_URL}/api/v1/enrollments" \
  -H 'Content-Type: application/json' \
  -d "$(CUSTOMER_ID="$CUSTOMER_ID" CUSTOMER_NAME="$CUSTOMER_NAME" SITE_ID="$SITE_ID" \
        SITE_NAME="$SITE_NAME" SITE_CITY="$SITE_CITY" python3 -c '
import json, os
print(json.dumps({
    "customer_id": os.environ["CUSTOMER_ID"],
    "customer_name": os.environ["CUSTOMER_NAME"],
    "site_id": os.environ["SITE_ID"],
    "site_name": os.environ["SITE_NAME"],
    "city": os.environ["SITE_CITY"],
}))')")"
enrollment_token="$(printf '%s' "$enrollment" | json '["enrollment_token"]')"
echo ok

printf 'enroll gateway %s... ' "$GATEWAY_ID"
gateway="$(curl -fsS -X POST "${API_URL}/api/v1/gateways/enroll" \
  -H 'Content-Type: application/json' \
  -d "$(ENROLLMENT_TOKEN="$enrollment_token" GATEWAY_ID="$GATEWAY_ID" python3 -c '
import json, os
print(json.dumps({
    "enrollment_token": os.environ["ENROLLMENT_TOKEN"],
    "gateway_id": os.environ["GATEWAY_ID"],
    "hostname": "demo-host",
    "version": "demo",
}))')")"
gateway_token="$(printf '%s' "$gateway" | json '["gateway_token"]')"
plan="$(printf '%s' "$gateway" | json '["entitlement"]["plan"]')"
limit="$(printf '%s' "$gateway" | json '["entitlement"]["camera_limit"]')"
echo "ok (plan ${plan}, camera limit ${limit})"

printf 'post %s cameras... ' "$CAMERAS"
batch="$(CAMERAS="$CAMERAS" GATEWAY_ID="$GATEWAY_ID" CUSTOMER_ID="$CUSTOMER_ID" \
  CUSTOMER_NAME="$CUSTOMER_NAME" SITE_ID="$SITE_ID" SITE_NAME="$SITE_NAME" \
  SITE_CITY="$SITE_CITY" python3 -c '
import datetime, json, os
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
gateway_id = os.environ["GATEWAY_ID"]
site_id = os.environ["SITE_ID"]
cameras = [{
    "camera_id": f"cam-demo-{index}",
    "gateway_id": gateway_id,
    "site_id": site_id,
    "name": f"Demo camera {index}",
    "status": "healthy",
    "manufacturer": "Demo",
    "model": "RTSP",
    "firmware": None,
    "profile_name": "Main",
    "codec": "h264",
    "width": 1920,
    "height": 1080,
    "fps": 25.0,
    "bitrate_kbps": 1800,
    "packet_loss": 0,
    "reconnects": 0,
    "rtsp_endpoint": f"rtsp://camera-{index}.demo/stream",
    "last_seen": now,
    "last_error": None,
} for index in range(1, int(os.environ["CAMERAS"]) + 1)]
print(json.dumps({
    "gateway_id": gateway_id,
    "customer_id": os.environ["CUSTOMER_ID"],
    "customer_name": os.environ["CUSTOMER_NAME"],
    "site_id": site_id,
    "site_name": os.environ["SITE_NAME"],
    "city": os.environ["SITE_CITY"],
    "sent_at": now,
    "cameras": cameras,
}))')"
curl -fsS -X POST "${API_URL}/api/v1/cameras/telemetry" \
  -H "Authorization: Bearer ${gateway_token}" \
  -H 'Content-Type: application/json' \
  -d "$batch" >/dev/null
echo ok

kept="$(curl -fsS -b "$cookie_jar" "${API_URL}/api/v1/fleet" | python3 -c '
import json, sys
fleet = json.load(sys.stdin)
print(sum(len(site["cameras"]) for customer in fleet["customers"] for site in customer["sites"]))')"
edition="$(curl -fsS "${API_URL}/api/v1/system/edition")"
echo
echo "plan:            $(printf '%s' "$edition" | json '["plan"]') ($(printf '%s' "$edition" | json '["edition"]'))"
echo "camera limit:    $(printf '%s' "$edition" | json '["camera_limit"]')"
echo "cameras sent:    ${CAMERAS}"
echo "cameras in fleet: ${kept}"
if [[ "$kept" != "$CAMERAS" ]]; then
  echo
  echo "The plan's limit dropped $((CAMERAS - kept)) of them at ingress: the API stores"
  echo "what the entitlement allows and says nothing to the gateway about the rest."
fi
