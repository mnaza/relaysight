#!/usr/bin/env bash
# Back up a running control plane, destroy it, restore it, and upgrade an old
# database with the current binary.
#
# Nobody had ever done any of this. What it is really for is the two things
# that are easy to get wrong: taking a copy of a database that is being
# written to, and opening an installation from before users existed with a
# binary that expects them.
set -euo pipefail

cd "$(dirname "$0")/.."

command -v sqlite3 >/dev/null || { echo "sqlite3 is not on PATH; this check needs it." >&2; exit 1; }

PASSWORD="a long enough password"
work="$(mktemp -d)"
api_pid=""
cleanup() {
    [ -n "$api_pid" ] && kill "$api_pid" 2>/dev/null || true
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

start_api() {  # database path -> sets api_pid, API
    local db="$1"
    port="$(free_port)"
    API="http://127.0.0.1:$port"
    mkdir -p "$work/plugins.d"
    API_BIND="127.0.0.1:$port" \
    DATABASE_URL="sqlite:$db" \
    ADMIN_PASSWORD="$PASSWORD" \
    PLUGIN_DIR="$work/plugins.d" \
    GATEWAY_TOKEN=a-gateway-token \
        ./target/debug/vms-api > "$work/api-$(basename "$db").log" 2>&1 &
    api_pid=$!
    for _ in $(seq 1 60); do curl -fs -o /dev/null "$API/healthz" && return 0; sleep 0.5; done
    echo "the API never came up on $db" >&2; cat "$work/api-$(basename "$db").log" >&2; exit 1
}
stop_api() { [ -n "$api_pid" ] && kill "$api_pid" 2>/dev/null; wait "$api_pid" 2>/dev/null || true; api_pid=""; }

owner_cookie() {
    curl -sS -D- -o /dev/null -X POST -H 'content-type: application/json' \
        --data "$(python3 -c "import json;print(json.dumps({'email':'admin@localhost','password':'$PASSWORD'}))")" \
        "$API/api/v1/auth/login" | grep -i '^set-cookie' | sed 's/.*: //; s/;.*//'
}

echo "== a control plane with something in it"
cargo build -q -p vms-api
start_api "$work/live.db"
cookie="$(owner_cookie)"
curl -fsS -o /dev/null -X POST -H "cookie: $cookie" -H 'content-type: application/json' \
    -d '{"email":"technician@example.test","password":"another long password","role":"technician"}' \
    "$API/api/v1/users"
curl -fsS -o /dev/null -X POST -H 'content-type: application/json' -H 'authorization: Bearer a-gateway-token' \
    -d "$(python3 - <<'PY'
import json, datetime
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
print(json.dumps({"gateway_id":"gw-restore","customer_id":"cust-1","customer_name":"Customer",
 "site_id":"site-1","site_name":"Site","city":"Town","sent_at":now,
 "cameras":[{"camera_id":"cam-1","gateway_id":"gw-restore","site_id":"site-1","name":"Front door",
   "status":"healthy","packet_loss":0,"reconnects":0,"last_seen":now}]}))
PY
)" "$API/api/v1/cameras/telemetry"

count() { curl -sS -H "cookie: $1" "$API$2" | python3 -c "
import sys,json
d=json.load(sys.stdin)
rows=d if isinstance(d,list) else next((v for v in d.values() if isinstance(v,list)), [])
print(len(rows))"; }
users_before="$(count "$cookie" /api/v1/users)"
[ "$users_before" = "2" ] || { echo "expected two users before the backup, got $users_before" >&2; exit 1; }
echo "   two users, one gateway, one camera"

echo "== sqlite3 .backup takes a consistent copy of a live database"
# Not `cp`: a plain copy of a database that is being written to can catch it
# between the pages of one transaction, and nothing tells you at the time.
# `.backup` goes through SQLite's own backup API, which takes the read lock it
# needs. `cp` is for a control plane that has been stopped.
sqlite3 "$work/live.db" ".backup '$work/backup.db'"
backup_users=$(sqlite3 "$work/backup.db" "SELECT COUNT(*) FROM users")
[ "$backup_users" = "$users_before" ] \
    || { echo "the backup has $backup_users of $users_before users" >&2; exit 1; }
echo "   $backup_users users in the backup, taken while the API was serving"

echo "== destroy, restore, and see whether the fleet came back"
stop_api
rm -f "$work/live.db" "$work/live.db-wal" "$work/live.db-shm"
cp "$work/backup.db" "$work/restored.db"
start_api "$work/restored.db"
cookie="$(owner_cookie)"
users_after="$(count "$cookie" /api/v1/users)"
[ "$users_after" = "$users_before" ] || { echo "$users_after users after the restore, $users_before before" >&2; exit 1; }
curl -sS -H "cookie: $cookie" "$API/api/v1/fleet" | grep -q "Front door" \
    || { echo "the camera did not come back into the fleet" >&2; exit 1; }
curl -sS -H "cookie: $cookie" "$API/api/v1/gateways" | grep -q "gw-restore" \
    || { echo "the gateway did not come back onto the roster" >&2; exit 1; }
verify="$(curl -sS -H "cookie: $cookie" "$API/api/v1/audit/verify")"
echo "$verify" | grep -q '"broken_at":null' \
    || { echo "the audit chain does not verify after a restore: $verify" >&2; exit 1; }
echo "$verify" | grep -q '"unchained":0' \
    || { echo "rows the chain cannot vouch for survived the restore: $verify" >&2; exit 1; }
echo "   users, the camera, the gateway roster, and an audit chain that still verifies"
stop_api

echo "== a database from before users existed, opened by today's binary"
old="$work/old.db"
for migration in services/api/migrations/000[1-9]_*.sql services/api/migrations/0010_*.sql; do
    sqlite3 "$old" < "$migration"
done
# sqlx keeps its own record of what it has applied; an old installation has
# one, so the new binary must not try to apply 0001 again.
sqlite3 "$old" "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL,
 installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL,
 checksum BLOB NOT NULL, execution_time BIGINT NOT NULL);"
python3 - "$old" <<'PY'
import hashlib, pathlib, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
for path in sorted(pathlib.Path("services/api/migrations").glob("0*.sql")):
    version = int(path.name.split("_", 1)[0])
    if version > 10:
        break
    body = path.read_bytes()
    db.execute(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)"
        " VALUES (?, ?, 1, ?, 0)",
        (version, path.stem.split("_", 1)[1].replace("_", " "), hashlib.sha384(body).digest()),
    )
db.commit()
PY
# The one thing an upgrade must not lose: the single password that was the
# only way in before there were users.
sqlite3 "$old" "INSERT INTO admin_credential (id, password_hash, updated_at)
 VALUES (1, '\$argon2id\$v=19\$m=19456,t=2,p=1\$c2FsdHlzYWx0eXNhbHR5\$8Q5rPmuQyfGV5Jp8i4Q1r1NCJmJ4g0kOZ3sHbYyHlPk', '2026-01-01T00:00:00Z');"
# A site with a gateway on it, written the way the old schema wanted it, so
# the upgrade has something of the fleet to carry across as well.
sqlite3 "$old" "INSERT INTO organizations (id, name) VALUES ('cust-1','Customer');
 INSERT INTO sites (id, org_id, name, city) VALUES ('site-1','cust-1','Site','Town');
 INSERT INTO gateways (id, site_id, last_seen) VALUES ('gw-old','site-1','2026-01-01T00:00:00Z');"

start_api "$old"
applied=$(sqlite3 "$old" "SELECT COUNT(*) FROM _sqlx_migrations")
total=$(ls services/api/migrations/*.sql | wc -l)
[ "$applied" = "$total" ] || { echo "$applied of $total migrations after the upgrade" >&2; exit 1; }
sqlite3 "$old" "SELECT id FROM gateways" | grep -qx gw-old \
    || { echo "the old gateway did not survive the upgrade" >&2; exit 1; }
owners=$(sqlite3 "$old" "SELECT COUNT(*) FROM users WHERE role = 'owner'")
[ "$owners" = "1" ] || { echo "the old single password did not become one owner (found $owners)" >&2; exit 1; }
carried=$(sqlite3 "$old" "SELECT password_hash FROM users WHERE role = 'owner'")
case "$carried" in
    '$argon2id$v=19$m=19456,t=2,p=1$c2FsdHlzYWx0eXNhbHR5$8Q5rPmuQyfGV5Jp8i4Q1r1NCJmJ4g0kOZ3sHbYyHlPk')
        echo "   $applied migrations applied, and the old password came across unchanged" ;;
    *)
        echo "the old password hash was replaced rather than carried over" >&2; exit 1 ;;
esac
stop_api

echo
echo "Backed up a live database, restored it, and upgraded one from before users existed."
