#!/usr/bin/env bash
# Is the relay usable over TLS right now?
#
# A handshake on 443 only says the listener is up. A browser will still refuse
# a turns: URL whose certificate has expired or belongs to another name, and a
# certbot renewal that quietly stopped working leaves exactly that behind — so
# the relay has to be unhealthy then too, not merely reachable.
#
# Runs as the container's healthcheck; see docker-compose.yml.
set -uo pipefail

realm="${RTC_TURN_REALM:-}"
servername=()
[[ -n "$realm" ]] && servername=(-servername "$realm")

cert="$(timeout 5 openssl s_client -connect 127.0.0.1:443 "${servername[@]}" </dev/null 2>/dev/null \
  | openssl x509 2>/dev/null)"
if [[ -z "$cert" ]]; then
  echo "no TLS handshake on 443"
  exit 1
fi

if ! printf '%s\n' "$cert" | openssl x509 -noout -checkend 0 >/dev/null 2>&1; then
  echo "the certificate served on 443 has expired"
  exit 1
fi

# Without a realm there is no name to hold it to, and the handshake stands on
# its own. With one, the certificate has to be for it.
#
# Read from the message, not the exit status: the openssl in this image (3.0.15)
# prints "does NOT match certificate" and still exits 0, so a name check on the
# status alone passes everything. 3.5 does set the status, and prints the same
# two messages; if a later one ever reworded them, this reads as unhealthy —
# the safe direction, and `make check-relay` step 9 would catch it.
if [[ -n "$realm" ]]; then
  match="$(printf '%s\n' "$cert" | openssl x509 -noout -checkhost "$realm" 2>/dev/null)"
  if [[ "$match" != *"does match certificate"* ]]; then
    echo "the certificate served on 443 is not for $realm"
    exit 1
  fi
fi

exit 0
