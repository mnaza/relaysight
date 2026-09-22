#!/usr/bin/env bash
# The release signing key, and the two forms of its public half.
#
#   scripts/release-key.sh generate KEY_PEM   # a new Ed25519 key, mode 0600
#   scripts/release-key.sh public KEY_PEM     # what the gateway and install.sh embed
#
# The gateway compiles in the raw 32-byte public key in base64 (RELEASE_PUBKEY
# in edge/gateway/src/update.rs, or GATEWAY_RELEASE_PUBKEY at build time).
# install.sh verifies with openssl, which wants PEM. Both come from one key;
# the private half never leaves wherever it was generated, apart from the CI
# secret that signs releases.
set -euo pipefail

case "${1:-}" in
  generate)
    key="${2:?usage: $0 generate KEY_PEM}"
    [[ -e "$key" ]] && { echo "$key exists; refusing to overwrite a key" >&2; exit 1; }
    (umask 077 && openssl genpkey -algorithm ed25519 -out "$key")
    echo "generated $key"
    ;;
  public)
    key="${2:?usage: $0 public KEY_PEM}"
    echo "gateway (RELEASE_PUBKEY / GATEWAY_RELEASE_PUBKEY):"
    openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | base64 -w0
    echo
    echo
    echo "install.sh (PUBKEY_PEM):"
    openssl pkey -in "$key" -pubout
    ;;
  *)
    echo "usage: $0 generate|public KEY_PEM" >&2
    exit 2
    ;;
esac
