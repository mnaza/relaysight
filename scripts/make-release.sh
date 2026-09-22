#!/usr/bin/env bash
# Assemble a signed gateway release from built binaries.
#
#   scripts/make-release.sh VERSION OUT_DIR KEY_PEM BASE_URL x86_64=PATH [aarch64=PATH]
#
# OUT_DIR gets, for each binary, vms-gateway-<arch>-linux; release.json, which
# names them under BASE_URL with their SHA-256; and release.json.sig, the
# Ed25519 signature over release.json's exact bytes in base64. That pair is
# what `vms-gateway update` and install.sh believe or refuse. KEY_PEM is a
# PKCS#8 Ed25519 private key — scripts/release-key.sh makes one.
#
# release.json puts each binary on a line of its own on purpose: install.sh
# reads it with sed on boxes that may have neither jq nor python, and since
# the manifest is signed, its layout is ours to rely on.
set -euo pipefail

usage() { echo "usage: $0 VERSION OUT_DIR KEY_PEM BASE_URL ARCH=PATH..." >&2; exit 2; }
[[ $# -ge 5 ]] || usage
version="$1" out="$2" key="$3" base_url="${4%/}"
shift 4

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must be X.Y.Z, got $version" >&2; exit 2; }
[[ -r "$key" ]] || { echo "cannot read key $key" >&2; exit 2; }

mkdir -p "$out"
entries=()
for pair in "$@"; do
  arch="${pair%%=*}" path="${pair#*=}"
  case "$arch" in x86_64|aarch64) ;; *) echo "unknown architecture $arch" >&2; exit 2 ;; esac
  [[ -f "$path" ]] || { echo "no binary at $path" >&2; exit 2; }
  name="vms-gateway-${arch}-linux"
  install -m 0755 "$path" "$out/$name"
  sha="$(sha256sum "$out/$name" | cut -d' ' -f1)"
  entries+=("\"$arch\":{\"url\":\"$base_url/$name\",\"sha256\":\"$sha\"}")
done

{
  printf '{"version":"%s","binaries":{\n' "$version"
  for i in "${!entries[@]}"; do
    sep=","; [[ $i -eq $(( ${#entries[@]} - 1 )) ]] && sep=""
    printf '%s%s\n' "${entries[$i]}" "$sep"
  done
  printf '}}\n'
} >"$out/release.json"

openssl pkeyutl -sign -inkey "$key" -rawin -in "$out/release.json" \
  | base64 -w0 >"$out/release.json.sig"

# Check our own work the way a box will: a release that does not verify
# should never leave this script.
pub="$(mktemp)"; sig="$(mktemp)"
trap 'rm -f "$pub" "$sig"' EXIT
openssl pkey -in "$key" -pubout -out "$pub"
base64 -d "$out/release.json.sig" >"$sig"
openssl pkeyutl -verify -pubin -inkey "$pub" -rawin -in "$out/release.json" -sigfile "$sig" >/dev/null

echo "release $version in $out:"
ls -1 "$out"
