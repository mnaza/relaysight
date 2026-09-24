#!/usr/bin/env bash
# Does a plugin call actually present a client certificate?
#
# The code that loads one is unit-tested; what that cannot show is whether the
# certificate reaches the far end. This runs a TLS plugin that demands client
# auth and checks that the conformance check gets in with a certificate and
# is refused without one.
set -euo pipefail

command -v openssl >/dev/null || { echo "openssl is needed" >&2; exit 1; }
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; [ -n "${PLUGIN_PID:-}" ] && kill "$PLUGIN_PID" 2>/dev/null || true' EXIT
PORT="${PORT:-19443}"

echo "== certificates"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$WORK/ca.key" -out "$WORK/ca.pem" \
    -subj "/CN=conformance-ca" -days 1 2>/dev/null
for who in server client; do
    openssl req -newkey rsa:2048 -nodes -keyout "$WORK/$who.key" -out "$WORK/$who.csr" \
        -subj "/CN=localhost" 2>/dev/null
    openssl x509 -req -in "$WORK/$who.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
        -CAcreateserial -out "$WORK/$who.crt" -days 1 \
        -extfile <(printf 'subjectAltName=DNS:localhost,IP:127.0.0.1') 2>/dev/null
done
cat "$WORK/client.crt" "$WORK/client.key" > "$WORK/client-identity.pem"

echo "== a plugin that demands a client certificate"
python3 - "$WORK" "$PORT" <<'PY' &
import json, ssl, sys, http.server
work, port = sys.argv[1], int(sys.argv[2])
MANIFEST = {"id": "mtls-plugin", "name": "mTLS", "version": "0.1.0", "protocol_version": 1,
            "vendor": "test", "description": "", "capabilities": ["event_sink"]}
class H(http.server.BaseHTTPRequestHandler):
    def _reply(self, body):
        raw = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)
    def do_GET(self):
        self._reply(MANIFEST if self.path.endswith("manifest")
                    else {"status": "ok", "plugin_id": "mtls-plugin", "details": None})
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self._reply({"delivered": True, "detail": "over mTLS"})
    def log_message(self, *a): pass
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(f"{work}/server.crt", f"{work}/server.key")
context.load_verify_locations(f"{work}/ca.pem")
context.verify_mode = ssl.CERT_REQUIRED          # the point of the exercise
server = http.server.HTTPServer(("127.0.0.1", port), H)
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
PY
PLUGIN_PID=$!
sleep 2

echo "== with a client certificate"
PLUGIN_CLIENT_IDENTITY="$WORK/client-identity.pem" PLUGIN_CA_BUNDLE="$WORK/ca.pem" \
    cargo run -q -p vms-plugin-runtime --example conformance -- "https://localhost:$PORT"

echo "== without one"
if PLUGIN_CA_BUNDLE="$WORK/ca.pem" \
    cargo run -q -p vms-plugin-runtime --example conformance -- "https://localhost:$PORT" \
    >"$WORK/no-cert.log" 2>&1; then
    echo "a plugin demanding client auth let us in without a certificate" >&2
    cat "$WORK/no-cert.log" >&2
    exit 1
fi
echo "   refused, as it should be"

echo "== a CA bundle with nothing in it"
: > "$WORK/empty.pem"
if PLUGIN_CA_BUNDLE="$WORK/empty.pem" \
    cargo run -q -p vms-plugin-runtime --example conformance -- "https://localhost:$PORT" \
    >"$WORK/empty-ca.log" 2>&1; then
    echo "an empty CA bundle was accepted" >&2
    exit 1
fi
echo "   refused, rather than trusting nothing quietly"

echo "mTLS holds up"
