#!/bin/sh
# Install the RelaySight gateway on a site box, or bring an existing install up
# to the latest release.
#
#   curl -fsSL https://github.com/mnaza/relaysight/releases/latest/download/install.sh \
#     | sudo sh -s -- --api-url https://api.example.com \
#                     --enrollment-token TOKEN --gateway-id site-01
#
# What it does, in order:
#   - fetches the release manifest and its signature, and refuses to go on
#     unless the signature is the release key's (openssl verifies it);
#   - downloads this architecture's binary and checks it against the hash the
#     signed manifest gives, then installs it as /usr/local/bin/vms-gateway;
#   - creates the system user relaysight-gateway, its state directory
#     /var/lib/relaysight-gateway (0700) and /etc/relaysight/gateway.env (0640);
#   - installs and starts relaysight-gateway.service, and a daily
#     relaysight-gateway-update.timer that runs `vms-gateway update`.
#
# No camera password goes on this command line. CAMERA_USERNAME and
# CAMERA_PASSWORD in gateway.env stay the optional fallback; a camera's own
# password goes in with `relaysight-gateway-credentials set HOST USER`, which
# reads it from stdin.
#
# Options:
#   --api-url URL            the control plane (required on first install)
#   --enrollment-token TOK   from the dashboard (required on first install)
#   --gateway-id ID          this gateway's name (required on first install)
#   --env KEY=VALUE          any other gateway setting; repeatable
#   --channel URL            where releases come from (a private mirror, a test)
#   --pubkey-file FILE       the release public key (PEM), for such a channel
#
# Re-running it keeps /etc/relaysight/gateway.env, changing only the keys it is
# given, and replaces the binary only when the release is a different version.
# See docs/INSTALL-GATEWAY.md.
set -eu

CHANNEL="https://github.com/mnaza/relaysight/releases/latest/download"
# The release public key. Empty until the release key exists; until then only a
# channel given with --pubkey-file can be installed from.
PUBKEY_PEM=''

BIN=/usr/local/bin/vms-gateway
STATE=/var/lib/relaysight-gateway
ETC=/etc/relaysight
ENV_FILE="$ETC/gateway.env"
SERVICE_USER=relaysight-gateway
UNITS=/etc/systemd/system

say() { printf 'install: %s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

# Everything runs from main, called on the last line: a download cut short
# by the network defines half a function and runs nothing.
main() {
api_url="" enrollment_token="" gateway_id="" pubkey_file="" extra_env=""
while [ $# -gt 0 ]; do
  case "$1" in
    --api-url) api_url="${2:?--api-url needs a value}"; shift 2 ;;
    --enrollment-token) enrollment_token="${2:?--enrollment-token needs a value}"; shift 2 ;;
    --gateway-id) gateway_id="${2:?--gateway-id needs a value}"; shift 2 ;;
    --channel) CHANNEL="${2:?--channel needs a value}"; shift 2 ;;
    --pubkey-file) pubkey_file="${2:?--pubkey-file needs a value}"; shift 2 ;;
    --env)
      case "${2:-}" in *=*) ;; *) die "--env takes KEY=VALUE, got '${2:-}'" ;; esac
      extra_env="$extra_env
$2"
      shift 2
      ;;
    -h|--help) sed -n '2,36p' "$0" 2>/dev/null || true; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done
CHANNEL="${CHANNEL%/}"

[ "$(id -u)" = 0 ] || die "run as root (it installs a service and a system user)"
[ -d /run/systemd/system ] || die "this box does not run systemd"
for tool in curl openssl sha256sum base64 useradd systemctl; do
  command -v "$tool" >/dev/null 2>&1 || die "needs $tool"
done

case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "no release for $(uname -m)" ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- believe the release before touching anything ---------------------------

if [ -n "$pubkey_file" ]; then
  cp "$pubkey_file" "$work/release.pub.pem" || die "cannot read $pubkey_file"
elif [ -n "$PUBKEY_PEM" ]; then
  printf '%s\n' "$PUBKEY_PEM" >"$work/release.pub.pem"
else
  die "this install.sh carries no release key; give one with --pubkey-file"
fi

curl -fsSL "$CHANNEL/release.json" -o "$work/release.json" || die "cannot fetch $CHANNEL/release.json"
curl -fsSL "$CHANNEL/release.json.sig" -o "$work/release.json.sig" || die "cannot fetch the signature"
base64 -d "$work/release.json.sig" >"$work/release.json.sig.bin" 2>/dev/null \
  || die "the release signature is not base64"
openssl pkeyutl -verify -pubin -inkey "$work/release.pub.pem" -rawin \
    -in "$work/release.json" -sigfile "$work/release.json.sig.bin" >/dev/null 2>&1 \
  || die "the release manifest is not signed by the release key; nothing was installed"

# The manifest is ours and signed, one binary per line (scripts/make-release.sh).
version="$(sed -n 's/^{"version":"\([0-9.]*\)".*/\1/p' "$work/release.json")"
line="$(grep "^\"$arch\":" "$work/release.json" || true)"
[ -n "$version" ] && [ -n "$line" ] || die "release has no binary for $arch"
url="$(printf '%s' "$line" | sed 's/.*"url":"\([^"]*\)".*/\1/')"
sha="$(printf '%s' "$line" | sed 's/.*"sha256":"\([0-9a-f]*\)".*/\1/')"

installed=""
[ -x "$BIN" ] && installed="$("$BIN" --version 2>/dev/null | sed -n 's/^vms-gateway //p')" || true

if [ "$installed" = "$version" ]; then
  say "vms-gateway $version is already installed"
else
  say "fetching vms-gateway $version for $arch"
  curl -fsSL "$url" -o "$work/vms-gateway" || die "cannot fetch $url"
  printf '%s  %s\n' "$sha" "$work/vms-gateway" | sha256sum -c - >/dev/null 2>&1 \
    || die "the downloaded binary does not match the signed manifest; nothing was installed"
fi

# --- the service's user, directories and configuration ----------------------

if ! id "$SERVICE_USER" >/dev/null 2>&1; then
  useradd --system --home-dir "$STATE" --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
fi
install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE"
install -d -m 0755 "$ETC"
if [ ! -f "$ENV_FILE" ]; then
  [ -n "$api_url" ] && [ -n "$enrollment_token" ] && [ -n "$gateway_id" ] \
    || die "a first install needs --api-url, --enrollment-token and --gateway-id"
  install -m 0640 -g "$SERVICE_USER" /dev/null "$ENV_FILE"
fi

# Values are single-quoted, which both systemd and sh read the same way.
set_env() {
  # A variable name and nothing else: the file is read by systemd and sourced
  # as shell by the credentials wrapper, and the name is a grep pattern below.
  case "$1" in ''|[0-9]*|*[!A-Za-z0-9_]*) die "'$1' is not a setting name" ;; esac
  case "$2" in *"'"*) die "$1 cannot contain a single quote" ;; esac
  grep -v "^$1=" "$ENV_FILE" >"$work/env" || true
  printf "%s='%s'\n" "$1" "$2" >>"$work/env"
  cat "$work/env" >"$ENV_FILE"
}
# `[ -z ] || …` rather than `[ -n ] && …`: under set -e an empty value must be a
# success, or a re-run with nothing to change dies at the first one.
[ -z "$api_url" ] || set_env API_URL "$api_url"
[ -z "$enrollment_token" ] || set_env ENROLLMENT_TOKEN "$enrollment_token"
[ -z "$gateway_id" ] || set_env GATEWAY_ID "$gateway_id"
set_env GATEWAY_STATE_DIR "$STATE"
set_env GATEWAY_UPDATE_URL "$CHANNEL"
printf '%s\n' "$extra_env" | while IFS= read -r pair; do
  [ -z "$pair" ] || set_env "${pair%%=*}" "${pair#*=}"
done

# --- the binary, then the units ---------------------------------------------

if [ "$installed" != "$version" ]; then
  install -m 0755 "$work/vms-gateway" "$BIN.new"
  mv -f "$BIN.new" "$BIN"
  say "installed vms-gateway $version"
fi

cat >"$UNITS/relaysight-gateway.service" <<'UNIT'
[Unit]
Description=RelaySight gateway
Documentation=https://github.com/mnaza/relaysight/blob/master/docs/INSTALL-GATEWAY.md
Wants=network-online.target
After=network-online.target

[Service]
User=relaysight-gateway
Group=relaysight-gateway
EnvironmentFile=/etc/relaysight/gateway.env
ExecStart=/usr/local/bin/vms-gateway
Restart=always
RestartSec=5

# It talks to cameras and to the API, and writes only its state directory.
NoNewPrivileges=yes
CapabilityBoundingSet=
AmbientCapabilities=
ProtectSystem=strict
ReadWritePaths=/var/lib/relaysight-gateway
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
RestrictSUIDSGID=yes
RestrictRealtime=yes
RestrictNamespaces=yes
LockPersonality=yes
ProtectHostname=yes
MemoryDenyWriteExecute=yes
# Netlink for enumerating interfaces when gathering ICE candidates.
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
SystemCallArchitectures=native

[Install]
WantedBy=multi-user.target
UNIT

cat >"$UNITS/relaysight-gateway-update.service" <<'UNIT'
[Unit]
Description=Update the RelaySight gateway to the latest signed release
Documentation=https://github.com/mnaza/relaysight/blob/master/docs/INSTALL-GATEWAY.md
Wants=network-online.target
After=network-online.target

[Service]
# Root: it replaces /usr/local/bin/vms-gateway and restarts the service. It
# keeps the new binary only once the gateway has heartbeated on it.
Type=oneshot
EnvironmentFile=/etc/relaysight/gateway.env
ExecStart=/usr/local/bin/vms-gateway update
UNIT

cat >"$UNITS/relaysight-gateway-update.timer" <<'UNIT'
[Unit]
Description=Check for a new RelaySight gateway release every day

[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
UNIT

cat >/usr/local/bin/relaysight-gateway-credentials <<'WRAPPER'
#!/bin/sh
# `vms-gateway credentials`, run the way the service runs: as its user, with
# its state directory and key, so the file it writes is the file the service
# reads. The password comes from stdin:
#
#   printf '%s\n' 'camera-password' | sudo relaysight-gateway-credentials set 192.168.1.50 admin
set -eu
[ "$(id -u)" = 0 ] || { echo "run with sudo: it reads /etc/relaysight/gateway.env" >&2; exit 1; }
set -a
. /etc/relaysight/gateway.env
set +a
exec runuser -u relaysight-gateway -- /usr/local/bin/vms-gateway credentials "$@"
WRAPPER
chmod 0755 /usr/local/bin/relaysight-gateway-credentials

systemctl daemon-reload
systemctl enable --quiet relaysight-gateway.service relaysight-gateway-update.timer
systemctl restart relaysight-gateway.service
systemctl start relaysight-gateway-update.timer

say "relaysight-gateway is running; updates are checked daily"
say "set a camera's password with: sudo relaysight-gateway-credentials set HOST USER"
}

main "$@"
