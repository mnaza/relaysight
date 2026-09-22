# Gateway installer and update channel

**Goal:** today a gateway runs from a source checkout with `docker compose
--profile edge`, and the dashboard's install command is a `docker run` that
puts the enrollment token and a camera password on the command line. Nothing
is released, so nothing can be installed on a site box and nothing keeps it
current. This gives the gateway a signed release, a one-line installer, a
hardened systemd service and a daily self-update that rolls back when the new
version does not come up.

Backlog: "Gateway installer package / update channel (systemd + Docker)".

**Decisions taken with the user (2026-09-22):**

- **A native binary under systemd.** No Docker needed on the box; ONVIF
  multicast works without host networking. The existing image is published
  from the same release for anyone who prefers it.
- **Self-update on a timer.** A daily timer checks the release channel,
  verifies, swaps atomically, restarts and rolls back on failure. No
  control-plane changes; central rollout can come later.
- **Retina from the public fork** until scottlamb/retina#137 lands, so a
  release works with the Dahua the gateway was proven against.
- **Ed25519 signatures checked by the gateway itself**, with the public key
  compiled in and `ring` doing the work. No extra tools on the box for
  updates.

## What exists today

- `edge/gateway/Dockerfile` builds `vms-gateway` from source; the compose
  `edge` profile runs it with host networking and `GATEWAY_STATE_DIR=/data`.
- The gateway is configured only by environment variables, has one
  subcommand (`credentials`), reports `CARGO_PKG_VERSION` (workspace
  `0.1.0`) in every heartbeat, and keeps its identity and camera credentials
  encrypted in `GATEWAY_STATE_DIR`.
- `retina` comes from crates.io, 0.4.20, and this machine patches it locally
  (`[patch.crates-io]`, uncommitted) with `mnaza/retina@83b25a8`, the commit
  behind #137. Without it the gateway cannot talk to that Dahua firmware;
  with it, every Docker build here breaks, because the patch points outside
  the build context.
- No release workflow, no published artifact. `VERSION` says `prototype-v6`.
- `web/brand.json`'s `installCommandTemplate` is the `docker run` above.

## Design

### Retina

`edge/gateway/Cargo.toml` depends on
`retina = { git = "https://github.com/mnaza/retina", rev = "83b25a8…" }`.
The local `[patch.crates-io]` goes, `publish.sh`'s refusal over it has
nothing left to refuse, and Docker builds here work again. When #137 lands
and is released, the dependency returns to crates.io.

### Versions

A release is `vX.Y.Z`, and the gateway's `CARGO_PKG_VERSION` is `X.Y.Z`. The
first release is `0.2.0`. An update only ever moves to a strictly greater
version: no downgrades, and a rollback is the updater's own act, never the
channel's.

### The release

Per architecture (`x86_64`, `aarch64`, Linux glibc), a bare binary
`vms-gateway-<arch>-linux`. Beside them:

- `release.json` — `{"version": "0.2.0", "binaries": {"x86_64": {"url": …,
  "sha256": …}, "aarch64": {…}}}`.
- `release.json.sig` — the raw Ed25519 signature of `release.json`, base64.

The manifest is what is signed; the binaries are pinned by their hashes in
it. `scripts/make-release.sh` builds all of that from binaries and a PEM key
with `openssl pkeyutl -sign -rawin`, so CI and the local check run the same
script.

The public key is compiled into the gateway. A build may replace it with
`GATEWAY_RELEASE_PUBKEY` set at compile time, which is how the installer
check signs with a throwaway key; a release build uses the real one.

### `vms-gateway update`

1. Fetch `release.json` and `release.json.sig` from the channel
   (`GATEWAY_UPDATE_URL`, default the public repository's latest release).
2. Verify the signature with `ring` against the compiled-in key. Anything
   else is an error that changes nothing.
3. If the version is not greater than its own, stop: up to date.
4. Download the binary for `std::env::consts::ARCH`, check its SHA-256.
5. Write it beside the running binary as `vms-gateway.new` (same
   filesystem), mode 0755; rename the current one to `vms-gateway.prev`, the
   new one into place.
6. Restart the service, then wait up to 90 s for the gateway to prove it is
   working: a heartbeat accepted by the API, which the gateway records by
   touching `GATEWAY_STATE_DIR/last-heartbeat` after each one.
7. If that never happens, put `vms-gateway.prev` back, restart, and exit
   non-zero. The next day's run tries again.

An API that is down during an update therefore reads as a bad release and is
rolled back. That is the safe mistake, and the timer retries.

The service that runs this is root (it replaces a file in `/usr/local/bin`);
the gateway itself never is.

### The installer

`deploy/gateway/install.sh`, run as root:

```
curl -fsSL https://…/install.sh | sudo sh -s -- \
  --api-url https://api.example.com --enrollment-token TOKEN --gateway-id site-01
```

- Detects the architecture, fetches the manifest and signature, and verifies
  them with `openssl pkeyutl -verify` against the public key embedded in the
  script. It refuses to continue if openssl is missing or cannot verify
  Ed25519.
- Downloads the binary, checks its hash, installs it to
  `/usr/local/bin/vms-gateway`.
- Creates a system user `relaysight-gateway`, `/var/lib/relaysight-gateway`
  (0700, the state directory) and `/etc/relaysight/gateway.env` (0640,
  root:relaysight-gateway) from the flags. No camera password goes in it:
  `CAMERA_USERNAME`/`CAMERA_PASSWORD` stay the optional fallback, and
  per-camera credentials go in with `vms-gateway credentials`.
- Installs `relaysight-gateway.service` (hardened: its own user,
  `ProtectSystem=strict`, `ReadWritePaths` for the state directory only,
  `NoNewPrivileges`, no capabilities), `relaysight-gateway-update.service`
  and a daily `relaysight-gateway-update.timer` with a randomised delay.
- A wrapper, `relaysight-gateway-credentials`, runs `vms-gateway
  credentials` as the service user with the service's state directory and
  key, so the file it writes is the file the service reads.
- Re-running it upgrades in place and keeps the env file unless flags change
  it.

### The dashboard

`installCommandTemplate` becomes the `curl … | sudo sh -s -- …` line, with
the same placeholders, and without the camera password.

### Publishing

`.github/workflows/release.yml` in the public repository: on a `v*` tag,
build both architectures, run `make-release.sh` with the private key from a
repository secret, create the GitHub release with the binaries, the
manifest, the signature and `install.sh`, and push the image to ghcr.io.
Creating the key, setting the secret and cutting the first tag are the
user's to do, or to approve.

## Testing

TDD as always.

Unit (`cargo test -p vms-gateway`): a manifest with a good signature
verifies, and each of a tampered manifest, a signature from another key, a
missing signature and a malformed manifest does not; the version comparison
refuses equal and older versions; the architecture picks its entry and a
missing one is an error; a hash mismatch is an error; the swap leaves
`.prev` and `.new` as described, and the rollback restores the previous
binary — driven through an injected restart-and-health function over temp
directories.

End to end (`make check-installer`): a systemd-booted Debian container, a
local HTTP release channel signed with a throwaway key, and a fake API that
accepts heartbeats. It checks four things:

- `install.sh` installs, and the service comes up and heartbeats.
- A newer release published to the channel is taken by the update service.
- A release whose binary exits at once is rolled back, and the old version
  keeps heartbeating.
- A manifest signed with the wrong key is refused, and nothing changes.

## Docs

- `docs/INSTALL-GATEWAY.md`: what the installer does, where everything
  lives, the update and rollback behaviour, how to set camera credentials,
  and how to uninstall.
- `docs/RUNNING-LOCALLY.md`: `make check-installer`.
- The private working notes that `publish.sh` never syncs: the retina patch is gone, and why.
- `docs/BACKLOG.md`: the line is checked off.

## Out of scope

Rollouts driven by the control plane, and a fleet view of versions. musl or
static builds; non-systemd init systems; Windows. An apt repository.
Delta updates. Automatic removal of `vms-gateway.prev` beyond keeping one.
