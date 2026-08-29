#!/usr/bin/env bash
set -euo pipefail

API_URL="${API_URL:-http://localhost:8080}"
CUSTOMER_SUFFIX="$(date +%s)"
GATEWAY_ID="smoke-gateway-${CUSTOMER_SUFFIX}"

printf '1/5 health... '
curl -fsS "${API_URL}/healthz" >/dev/null
echo ok

printf '2/5 create enrollment... '
ENROLLMENT_JSON="$(curl -fsS -X POST "${API_URL}/api/v1/enrollments" \
  -H 'Content-Type: application/json' \
  -d "{\"customer_id\":\"smoke-${CUSTOMER_SUFFIX}\",\"customer_name\":\"Smoke Customer\",\"site_id\":\"smoke-site-${CUSTOMER_SUFFIX}\",\"site_name\":\"Smoke Site\",\"city\":\"Madrid\"}")"
ENROLLMENT_TOKEN="$(printf '%s' "$ENROLLMENT_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["enrollment_token"])')"
echo "$ENROLLMENT_TOKEN"

printf '3/5 enroll gateway... '
GATEWAY_JSON="$(curl -fsS -X POST "${API_URL}/api/v1/gateways/enroll" \
  -H 'Content-Type: application/json' \
  -d "{\"enrollment_token\":\"${ENROLLMENT_TOKEN}\",\"gateway_id\":\"${GATEWAY_ID}\",\"hostname\":\"smoke-host\",\"version\":\"smoke\"}")"
GATEWAY_TOKEN="$(printf '%s' "$GATEWAY_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["gateway_token"])')"
echo ok

NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf '4/5 send camera telemetry... '
curl -fsS -X POST "${API_URL}/api/v1/cameras/telemetry" \
  -H "Authorization: Bearer ${GATEWAY_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d "{\"gateway_id\":\"${GATEWAY_ID}\",\"customer_id\":\"smoke-${CUSTOMER_SUFFIX}\",\"customer_name\":\"Smoke Customer\",\"site_id\":\"smoke-site-${CUSTOMER_SUFFIX}\",\"site_name\":\"Smoke Site\",\"city\":\"Madrid\",\"sent_at\":\"${NOW}\",\"cameras\":[{\"camera_id\":\"cam-smoke-1\",\"gateway_id\":\"${GATEWAY_ID}\",\"site_id\":\"smoke-site-${CUSTOMER_SUFFIX}\",\"name\":\"Smoke camera\",\"status\":\"healthy\",\"manufacturer\":\"Demo\",\"model\":\"RTSP\",\"firmware\":null,\"profile_name\":\"Main\",\"codec\":\"h264\",\"width\":1920,\"height\":1080,\"fps\":25.0,\"bitrate_kbps\":1800,\"packet_loss\":0,\"reconnects\":0,\"rtsp_endpoint\":\"rtsp://camera.example/stream\",\"last_seen\":\"${NOW}\",\"last_error\":null}]}" >/dev/null
echo ok

printf '5/5 verify live fleet... '
SOURCE="$(curl -fsS "${API_URL}/api/v1/fleet" | python3 -c 'import json,sys; print(json.load(sys.stdin)["source"])')"
if [[ "$SOURCE" != "live" ]]; then
  echo "expected live, got ${SOURCE}" >&2
  exit 1
fi
echo ok

echo "Smoke test passed."
