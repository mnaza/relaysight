#!/usr/bin/env bash
# Sign in through Keycloak: a real identity provider, not the one in this
# repository.
#
# `make check-sso` proves the protocol against a provider this repository
# wrote, which agrees with the code that expects it. This starts Keycloak,
# gives it a realm, a client and two users, and drives its own login form
# with curl. It is slower and it needs Docker; it is also the only thing here
# that has met a provider somebody else wrote.
#
# It found something the first time it was run: with autocreate on, an email
# Keycloak had *not* verified was let in and given an account.
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${KEYCLOAK_IMAGE:-quay.io/keycloak/keycloak:26.0}"
CONTAINER=relaysight-sso-check
KC_ADMIN_PASSWORD="an admin password"
CLIENT_SECRET="a secret keycloak knows"
USER_PASSWORD="a long enough password"

command -v docker >/dev/null || { echo "docker is not on PATH; this check needs it." >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "docker is not usable by this user." >&2; exit 1; }

work="$(mktemp -d)"
api_pid=""
cleanup() {
    [ -n "$api_pid" ] && kill "$api_pid" 2>/dev/null || true
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

kc_port="$(free_port)"
api_port="$(free_port)"
KC="http://localhost:$kc_port"
API="http://127.0.0.1:$api_port"
REDIRECT="http://localhost:$api_port/api/v1/auth/oidc/callback"

echo "== starting Keycloak on $kc_port"
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" -p "$kc_port:8080" \
    -e KC_BOOTSTRAP_ADMIN_USERNAME=admin \
    -e KC_BOOTSTRAP_ADMIN_PASSWORD="$KC_ADMIN_PASSWORD" \
    "$IMAGE" start-dev >/dev/null

for _ in $(seq 1 120); do
    curl -fs -o /dev/null "$KC/realms/master/.well-known/openid-configuration" && break
    sleep 1
done
curl -fs -o /dev/null "$KC/realms/master/.well-known/openid-configuration" \
    || { echo "Keycloak never came up" >&2; docker logs --tail 20 "$CONTAINER" >&2; exit 1; }

token=$(curl -s -d client_id=admin-cli -d username=admin --data-urlencode "password=$KC_ADMIN_PASSWORD" \
    -d grant_type=password "$KC/realms/master/protocol/openid-connect/token" \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['access_token'])")
auth="Authorization: Bearer $token"
kc_post() { curl -fsS -o /dev/null -X POST -H "$auth" -H 'content-type: application/json' "$1" -d "$2"; }

echo "== a realm, a client and two people"
kc_post "$KC/admin/realms" '{"realm":"relaysight","enabled":true}'
kc_post "$KC/admin/realms/relaysight/clients" "$(python3 -c "
import json,sys
print(json.dumps({'clientId':'relaysight-vms','enabled':True,'protocol':'openid-connect',
 'publicClient':False,'standardFlowEnabled':True,'directAccessGrantsEnabled':False,
 'secret':'$CLIENT_SECRET','redirectUris':['$REDIRECT']}))")"
new_user() {  # username, email, verified
    kc_post "$KC/admin/realms/relaysight/users" "$(python3 -c "
import json
print(json.dumps({'username':'$1','enabled':True,'email':'$2','emailVerified':$3,
 'firstName':'$1','lastName':'Person',
 'credentials':[{'type':'password','value':'$USER_PASSWORD','temporary':False}]}))")"
}
new_user verified verified@example.test True
new_user unverified unverified@example.test False

echo "== starting the control plane on $api_port, pointed at Keycloak"
cargo build -q -p vms-api
mkdir -p "$work/plugins.d"
API_BIND="127.0.0.1:$api_port" \
DATABASE_URL="sqlite:$work/vms.db" \
ADMIN_PASSWORD="$USER_PASSWORD" \
PLUGIN_DIR="$work/plugins.d" \
OIDC_ISSUER="$KC/realms/relaysight" \
OIDC_CLIENT_ID=relaysight-vms \
OIDC_CLIENT_SECRET="$CLIENT_SECRET" \
OIDC_REDIRECT_URL="$REDIRECT" \
OIDC_AUTOCREATE_ROLE=viewer \
    ./target/debug/vms-api > "$work/api.log" 2>&1 &
api_pid=$!
for _ in $(seq 1 60); do curl -fs -o /dev/null "$API/healthz" && break; sleep 1; done
curl -fs -o /dev/null "$API/healthz" || { echo "the API never came up" >&2; cat "$work/api.log" >&2; exit 1; }

# The browser's half: start, let Keycloak show its login form, fill it in.
sign_in() {  # username -> prints the session cookie, or nothing
    local user="$1" jar="$work/jar-$1" authorize page action
    rm -f "$jar"
    authorize=$(curl -sS -c "$jar" -b "$jar" -o /dev/null -w '%{redirect_url}' "$API/api/v1/auth/oidc/start")
    page=$(curl -sS -c "$jar" -b "$jar" -L "$authorize")
    action=$(echo "$page" | grep -o 'action="[^"]*"' | head -1 | sed 's/^action="//; s/"$//; s/&amp;/\&/g')
    [ -n "$action" ] || { echo "Keycloak showed no login form" >&2; exit 1; }
    curl -sS -c "$jar" -b "$jar" -L --max-redirs 8 -o "$work/final-$user.html" \
        --data-urlencode "username=$user" --data-urlencode "password=$USER_PASSWORD" "$action" || true
    # No cookie is an answer, not a failure: half of these cases are refusals.
    { grep vms_session "$jar" || true; } | awk '{print $6"="$7}'
}

as_owner() {
    curl -sS -D- -o /dev/null -X POST -H 'content-type: application/json' \
        --data "$(python3 -c "
import json
print(json.dumps({'email':'admin@localhost','password':'$USER_PASSWORD'}))")" \
        "$API/api/v1/auth/login" | grep -i '^set-cookie' | sed 's/.*: //; s/;.*//'
}
users_json() { curl -sS -H "cookie: $(as_owner)" "$API/api/v1/users"; }

echo "== a verified email signs in, and autocreate gives it the role it was told to"
session="$(sign_in verified)"
[ -n "$session" ] || { echo "Keycloak vouched for a verified user and no session came out" >&2
    tail -5 "$work/api.log" >&2; exit 1; }
code=$(curl -sS -o /dev/null -w '%{http_code}' -H "cookie: $session" "$API/api/v1/fleet")
[ "$code" = "200" ] || { echo "the session Keycloak minted does not work: $code" >&2; exit 1; }
users_json | grep -q '"verified@example.test"' || { echo "no account was created" >&2; exit 1; }
users_json | python3 -c "
import sys,json
rows=json.load(sys.stdin)
rows=rows if isinstance(rows,list) else rows.get('users',rows)
role=[u['role'] for u in rows if u['email']=='verified@example.test'][0]
assert role=='viewer', f'autocreate made a {role}'
print('   verified@example.test is a viewer, and the session reads the fleet')"

echo "== an email Keycloak has not verified is refused, and gets no account"
session="$(sign_in unverified)"
[ -z "$session" ] || { echo "an unverified email was signed in" >&2; exit 1; }
grep -q "not verified" "$work/final-unverified.html" \
    || { echo "refused, but not for the reason expected:" >&2; head -c 200 "$work/final-unverified.html" >&2; exit 1; }
if users_json | grep -q 'unverified@example.test'; then
    echo "an unverified email had an account created" >&2
    exit 1
fi
echo "   refused, and no account behind it"

echo "== an account turned off here is refused whatever Keycloak says"
owner="$(as_owner)"
uid=$(users_json | python3 -c "
import sys,json
rows=json.load(sys.stdin)
rows=rows if isinstance(rows,list) else rows.get('users',rows)
print([u['id'] for u in rows if u['email']=='verified@example.test'][0])")
curl -fsS -o /dev/null -X POST -H "cookie: $owner" -H 'content-type: application/json' \
    -d '{"disabled":true}' "$API/api/v1/users/$uid"
session="$(sign_in verified)"
[ -z "$session" ] || { echo "a disabled account signed in through the provider" >&2; exit 1; }
echo "   refused"

echo
echo "$IMAGE signed somebody in, and was refused three ways."
