# Per-camera credentials, encrypted at rest on the edge

**Goal:** every camera on a site shares one username and password today, read
from `CAMERA_USERNAME` and `CAMERA_PASSWORD` in the gateway's environment. Real
sites do not work that way — cameras arrive from different installers, at
different times, with different passwords — and an environment variable is a
poor place for any of them. This gives the gateway a per-camera credential
store, encrypted with the key that already protects its identity.

Backlog: "Per-camera credential store encrypted at rest on edge".

**Decisions taken with the user (2026-09-21):**

- **A `credentials` subcommand writes them.** The password is read from stdin,
  so it never reaches the environment, `ps`, or shell history.
- **Keyed by the camera's address.** Credentials are what a camera needs before
  it will answer the ONVIF call that assigns its id, so an id cannot be the key
  on first contact. `ONVIF_HOSTS` already names cameras this way.
- **`CAMERA_USERNAME` / `CAMERA_PASSWORD` stay as the fallback.** A host with no
  entry of its own uses them, so every deployment that exists keeps working and
  a site with one shared password needs no setup at all.

## What exists today

- `Config::from_env` (`edge/gateway/src/main.rs`) reads one optional pair and
  hands it to four places: ONVIF resolution (`onvif::resolve_camera`), the RTSP
  probe (`rtsp::probe`), and the `CameraSource` record that live sessions and
  recordings dial with.
- `CameraSource` already carries `username` and `password` per camera; they are
  simply copies of the one pair.
- `edge/gateway/src/identity.rs` keeps the gateway's enrollment secrets in
  `identity.enc`, XChaCha20-Poly1305 with a 24-byte nonce prepended, under a key
  from `GATEWAY_STATE_KEY` or a `identity.key` file created 0600 in a 0700 state
  directory.
- `onvif::xaddr_authority` turns a device's xaddr into `host:port`, which is how
  the hosts loop already de-duplicates discovery against `ONVIF_HOSTS`.
- The gateway parses no arguments at all: it is `main()` and environment
  variables.

## Design

### The store

New module `edge/gateway/src/camera_credentials.rs`, holding the file format
and nothing about how cameras are found.

- File `camera-credentials.enc` in the state directory beside `identity.enc`,
  written 0600, same construction: 24-byte nonce, XChaCha20-Poly1305, the key
  from `GATEWAY_STATE_KEY` or `identity.key`. `identity.rs` grows two
  `pub(crate)` helpers — `state_key(dir, env_key)` and the seal/open pair — so
  the crypto has one home rather than two.
- Plaintext is JSON: `{"version": 1, "cameras": {"<host>": {"username": …,
  "password": …}}}`. An unknown version is an error, never a guess, as with the
  identity file.
- `CameraCredentials::load(dir, env_key)` reads it (an absent file is an empty
  store, not an error), `set(host, username, password)`, `remove(host)` and
  `hosts()` — which returns hosts and usernames, never passwords.
- `Debug` prints usernames and the count; passwords render as `<redacted>`, as
  `GatewayIdentity` already does for its token.

### Keying and lookup

The key is the address as written, normalised: lowercased, whitespace trimmed,
any `scheme://`, userinfo and path removed, so `192.168.1.50`,
`192.168.1.50:80` and `http://192.168.1.50/onvif/device_service` all find one
entry. A bare host matches an entry with no port and vice versa; a host with a
port prefers the exact entry and falls back to the bare one. An IPv6 literal
keeps its brackets — `url::Url::host_str` reports them, so `[2001:db8::1]` is
the form on both sides — and its own colons are not a port separator.

`Config` carries the loaded store. The four call sites ask it for a host and
fall back to the environment pair:

- ONVIF discovery and `ONVIF_HOSTS` — the device's xaddr authority.
- The RTSP probe, live sessions and recordings — the authority of the RTSP URL
  being dialled, which is the same camera by a different port.

A camera whose address has no entry behaves exactly as it does today.

### The subcommand

`main()` looks at its arguments before anything else. With none, it runs the
gateway as now. With `credentials`:

```
vms-gateway credentials set <host> <username>   # password on stdin
vms-gateway credentials remove <host>
vms-gateway credentials list                    # hosts and usernames
```

Parsed by hand from `std::env::args()` — the gateway has no argument parser and
this does not earn one. `set` reads the password from stdin (one line, trailing
newline stripped) and refuses an empty one. Everything writes through the same
store, so the file it produces is the file the running gateway reads, and
`GATEWAY_STATE_DIR` / `GATEWAY_STATE_KEY` mean the same thing for both.

Anything else prints the usage above and exits non-zero.

## Testing

TDD as always.

Unit (`cargo test -p vms-gateway`):

- Round trip: set two hosts, reload from a fresh store, both come back; the
  file on disk is 0600 and contains neither password in plaintext.
- An absent file loads as an empty store; a corrupt one is an error; an unknown
  version is an error naming the version.
- Normalisation: `HTTP://192.168.1.50:80/onvif/device_service` and
  ` 192.168.1.50 ` reach the entry stored as `192.168.1.50:80`; a bare host
  matches a port-qualified entry only when no exact entry exists.
- Lookup: a host with an entry gets it; a host without falls back to the
  environment pair; with neither, nothing.
- `Debug` does not print a password, and `hosts()` does not return one.
- The subcommand: `set` then `list` shows the host and username and no
  password; `remove` takes it away; an empty password is refused; an unknown
  subcommand exits non-zero with the usage.

The existing gateway tests must pass untouched: with no credential file, the
gateway behaves exactly as before.

## Docs

- `docs/RUNNING-LOCALLY.md`: how to set a per-camera password, and that the
  environment pair is now the fallback rather than the only way.
- `README.md`: the `credentials` subcommand beside the existing edge setup.
- `docs/BACKLOG.md`: the line is checked off.

## Out of scope

Rotating credentials on a schedule, or any control-plane involvement — the
cloud holding camera passwords is a different trust story and a much larger
change. Per-camera credentials for anything but ONVIF and RTSP. Reading
credentials from a secret manager. A UI: this is a command on the box.
