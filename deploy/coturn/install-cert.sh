#!/usr/bin/env bash
# Install a certificate for the relay and make coturn pick it up.
#
# certbot runs this as a deploy hook, after the first issuance and after every
# renewal, with RENEWED_LINEAGE set to /etc/letsencrypt/live/<name>:
#
#   certbot certonly --standalone -d relay.example.com \
#     --deploy-hook /path/to/relaysight/deploy/coturn/install-cert.sh
#
# It copies instead of pointing coturn at /etc/letsencrypt: the files there are
# symlinks into archive/ and the key is readable by root only, while coturn
# runs as nobody. See docs/TURN-DEPLOY.md.
#
# Writes to CERT_DIR in its own environment (default: certs/ beside this
# script), group CERT_GROUP (default: 65534) — not to wherever the compose
# file's RTC_TURN_CERT_DIR points, and certbot's renewal timer runs this hook
# with an empty environment, so that variable never reaches it either way. If
# the compose mount was moved with RTC_TURN_CERT_DIR, register a small wrapper
# as the deploy hook instead: one that sets CERT_DIR (and CERT_GROUP, if it
# also differs) before calling this script.
set -euo pipefail

: "${RENEWED_LINEAGE:?set RENEWED_LINEAGE to the directory holding fullchain.pem and privkey.pem}"
CERT_DIR="${CERT_DIR:-$(cd "$(dirname "$0")" && pwd)/certs}"
CERT_GROUP="${CERT_GROUP:-65534}"

chain="$RENEWED_LINEAGE/fullchain.pem"
key="$RENEWED_LINEAGE/privkey.pem"
for file in "$chain" "$key"; do
  [[ -r "$file" ]] || { echo "install-cert: cannot read $file; nothing installed" >&2; exit 1; }
done

# A key that does not belong to the certificate takes TLS down with nothing but
# an unhealthy container to show for it. Refuse it here, before anything moves.
# Assignments, not inline substitutions, so an openssl failure stops the script
# instead of comparing two empty strings.
cert_public="$(openssl x509 -in "$chain" -noout -pubkey)"
key_public="$(openssl pkey -in "$key" -pubout)"
if [[ "$cert_public" != "$key_public" ]]; then
  echo "install-cert: $key does not belong to $chain; nothing installed" >&2
  exit 1
fi

mkdir -p "$CERT_DIR"
chgrp "$CERT_GROUP" "$CERT_DIR"
chmod 0750 "$CERT_DIR"

# Temporary names in the same directory, then rename: a reload never reads a
# file that is still being written.
tmp_chain="$(mktemp "$CERT_DIR/.fullchain.XXXXXX")"
tmp_key="$(mktemp "$CERT_DIR/.privkey.XXXXXX")"
trap 'rm -f "$tmp_chain" "$tmp_key"' EXIT
cat "$chain" >"$tmp_chain"
cat "$key" >"$tmp_key"
chmod 0644 "$tmp_chain"
chgrp "$CERT_GROUP" "$tmp_key"
chmod 0640 "$tmp_key"
mv -f "$tmp_chain" "$CERT_DIR/fullchain.pem"
mv -f "$tmp_key" "$CERT_DIR/privkey.pem"

relay="$(docker ps -q \
  --filter label=com.docker.compose.project=coturn \
  --filter label=com.docker.compose.service=coturn)"
if [[ -z "$relay" ]]; then
  echo "install-cert: installed in $CERT_DIR; coturn is not running and will read it when it starts"
  exit 0
fi
# SIGUSR2 makes coturn reload its certificate and key without a restart.
docker kill --signal SIGUSR2 "$relay" >/dev/null

# A reload is not a listener. coturn creates its TLS listeners at start or not
# at all: one that came up without a readable certificate stays closed on 443,
# and this signal changes nothing there. Reporting a reload then is the worst
# outcome — the certificate is in place, the relay looks tended to, and every
# site behind a TLS-only firewall still cannot connect.
listening() {
  docker exec "$relay" timeout 3 openssl s_client -connect 127.0.0.1:443 -brief </dev/null \
    >/dev/null 2>&1
}
for _ in 1 2 3 4 5; do
  listening && break
  sleep 1
done
if listening; then
  echo "install-cert: installed in $CERT_DIR and signalled coturn to reload"
  exit 0
fi
echo "install-cert: installed in $CERT_DIR, but nothing answers TLS on 443." >&2
echo "The relay was started without a readable certificate, so it has no TLS" >&2
echo "listener to reload. Restart it to open 443:" >&2
echo "  docker compose -f deploy/coturn/docker-compose.yml restart coturn" >&2
exit 1
