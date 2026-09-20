# Relay: TURN over TLS on 443

**Goal:** The relay exists for sites a direct path cannot reach, and the
strictest of them allow nothing outbound but TLS on 443. coturn is configured
with `tls-listening-port=5349` and no certificate, so no TLS listener has ever
come up. This makes coturn serve TURN over TLS on 443, installs and renews its
certificate without a restart, and makes a broken TLS listener visible instead
of silent.

This is the first of two parts of epic `relaysight-vms-8p3`. **It does not by
itself serve a camera site behind TLS-only egress**: the gateway's WebRTC stack
(webrtc-rs 0.20.3, and 0.20.5 upstream) skips every TURN URL that is not plain
UDP — `turn_relayer.rs` logs "Skipping unsupported secure TURN url" and
"Skipping unsupported non-UDP TURN url". Upstream tracks TCP as
webrtc-rs#848 (open, post-1.0); TLS is not in its scope. The second part adds
TURN over TCP and TLS to that relayer and needs this relay to test against.
Until then, `turns:` on 443 serves browsers — viewers behind strict egress.

**Decisions taken with the user (2026-09-11):**

- **Gateway gap closed by extending webrtc-rs**, carried as a local patch and
  offered upstream — not by an HTTPS media fallback. That is part two; this
  document is part one.
- **Certificates are mounted files plus a renewal hook** that certbot calls.
  No ACME container in compose.
- **coturn runs as `nobody` with the bind capability declared**, not as root
  dropping privileges and not on a bridge network.

## What exists today

- `deploy/coturn/turnserver.conf`: `use-auth-secret`, `listening-port=3478`,
  `tls-listening-port=5349`, `no-tlsv1`, `no-tlsv1_1`, the private-range peer
  blacklist, relay range 49160–49200. No `cert`/`pkey`.
- `deploy/coturn/docker-compose.yml`: `coturn/coturn:4.6`, host networking,
  realm/external IP/secret as required flags. No healthcheck, no capability
  settings, no certificate mount.
- `services/api/src/turn.rs` hands out whatever `RTC_TURN_URLS` lists with a
  minted credential; `turns:` URLs already pass through untouched. The browser
  builds `RTCPeerConnection` from that list. No API change is needed.
- `docs/TURN-DEPLOY.md` says the fix is `cert`/`pkey` plus
  `alt-tls-listening-port=443`. That fix does not work (below).
- No CI. Checks are shell scripts (`scripts/smoke-api.sh`) and Makefile
  targets (`check-web`).

## What was established locally (throwaway probes, 2026-09-11)

Docker 29.8, `coturn/coturn:4.6`, host networking, self-signed certificate:

- **`alt-tls-listening-port=443` never opens 443**, as `nobody` or as root.
  Alternative ports exist for RFC 5780 NAT-behaviour discovery.
  `tls-listening-port=443` does open it.
- **With a certificate, TLS listens.** A handshake on 443 completes with
  TLSv1.3. `turnutils_uclient -S -t -p 443` connects over TLS and its channel
  bind is refused with 403 (Forbidden IP) — coturn only evaluates the peer
  inside an authenticated allocation, and the shipped blacklist forbids the
  loopback peer.
- **`nobody` binds 443 because Docker grants `CAP_NET_BIND_SERVICE`**
  (`CapEff 0x400`, the only capability). Host networking keeps the host's
  `ip_unprivileged_port_start=1024`; a bridge namespace has 0. With
  `--cap-drop ALL --cap-add NET_BIND_SERVICE` it still binds 443 and 3478.
- **`SIGUSR2` reloads certificates and keys** without a restart ("Reloading
  TLS certificates and keys"; upstream ChangeLog "reload-tls-certs PR#236").
- **An unreadable key fails silently.** As `nobody` with the key at 0600
  owned by another user, coturn keeps running (exit 0), logs "cannot start TLS
  and DTLS listeners because private key file is not set properly", and serves
  only 3478 — the same shape as today's bug. As root-then-`--proc-user`, the
  same key passes startup and the first `SIGUSR2` fails with "invalid private
  key".
- The image has `bash`, `openssl` and `timeout`; no `curl`, no `nc`.

## Relay configuration

`deploy/coturn/turnserver.conf`:

- `tls-listening-port=443` replaces `5349`.
- `cert=/etc/coturn/certs/fullchain.pem` and
  `pkey=/etc/coturn/certs/privkey.pem`.
- `no-tlsv1` and `no-tlsv1_1` stay: TLS 1.2 is the minimum.
- 3478 over UDP and TCP is unchanged.

`deploy/coturn/docker-compose.yml`:

- `name: coturn` at the top, so the project name — and the labels the hook
  finds the container by — do not depend on the directory it is run from.
- User stays the image default, `nobody`. Host networking stays.
- `cap_drop: [ALL]` and `cap_add: [NET_BIND_SERVICE]`.
- `${RTC_TURN_CERT_DIR:-./certs}:/etc/coturn/certs:ro`. `deploy/coturn/certs/`
  is gitignored except a `.gitkeep`, so Docker never creates the default mount
  source as root. The override lets the local check use a temporary directory
  instead of overwriting real certificates.
- `group_add: ["${RTC_TURN_CERT_GROUP:-65534}"]` — redundant in production
  (65534 is already `nobody`'s group); lets the local check use the real
  permission rule with the caller's own group, without root.
- Healthcheck:
  `["CMD", "bash", "-c", "timeout 5 openssl s_client -connect 127.0.0.1:443 -brief </dev/null"]`,
  `interval: 10s`, `timeout: 8s`, `retries: 3`, `start_period: 10s`. Missing
  certificate, unreadable key and failed bind all leave coturn running with no
  TLS listener; each becomes `unhealthy` in `docker compose ps`.

The relay host must not already use 443 — consistent with the dedicated box
this compose file already assumes.

## Certificate install and renewal

`deploy/coturn/install-cert.sh`, used as certbot's `--deploy-hook` (certbot
runs it after first issuance and after every renewal) and runnable by hand.

Inputs:

- `RENEWED_LINEAGE` — required. The directory holding `fullchain.pem` and
  `privkey.pem` (certbot sets it to `/etc/letsencrypt/live/<name>`).
- `CERT_DIR` — default: the `certs/` directory beside the script.
- `CERT_GROUP` — default `65534`.

Behaviour, in order:

1. Refuse (non-zero exit, nothing changed) if either file is missing.
2. Refuse (non-zero exit, nothing changed) if the key does not belong to the
   certificate — their public keys are compared with `openssl`.
3. Write both into `CERT_DIR` via temporary files in that directory and
   rename, so a reload never reads a half-written pair. `fullchain.pem` 0644;
   `privkey.pem` 0640 with group `CERT_GROUP`. A failed `chgrp` is a non-zero
   exit.
4. Send `SIGUSR2` to the container labelled
   `com.docker.compose.project=coturn` and
   `com.docker.compose.service=coturn` via `docker kill`. Not
   `docker compose kill`: loading the compose file demands the realm, IP and
   secret, which certbot's environment does not have. If no such container is
   running (first issuance), say so and exit 0.

Why copy rather than mount `/etc/letsencrypt/live` as certbot's docs suggest:
those are symlinks into `archive/` and the key is root-only, while coturn runs
as `nobody`.

## Docs

- `docs/TURN-DEPLOY.md`: the example `RTC_TURN_URLS` adds
  `turns:relay.example.com:443?transport=tcp`; the firewall table replaces
  5349/TCP with 443/TCP; the TLS section is rewritten from the facts above
  (`tls-listening-port`, not the alternative port; `nobody` with the declared
  capability; key readable by coturn's group; the silent failure and the
  healthcheck that catches it; `SIGUSR2`); a certificate section with
  `certbot certonly --standalone -d <realm> --deploy-hook <path>/install-cert.sh`
  (port 80 open during issuance and renewal); a dated verified list and a
  not-verified list (a real domain with certbot; Docker or Podman setups that
  do not grant the capability); a section stating the gateway cannot use TURN
  over TCP or TLS yet, with webrtc-rs#848.
- `docs/RUNNING-LOCALLY.md` "Adding the relay": points to `make check-relay`.
- The private working notes that `publish.sh` never syncs (named on its line
  8), "Known not to work": corrected — the fix they point to did not work; say
  what does, and that camera sites still wait on the gateway part.
- `docs/BACKLOG.md` under "Next": "Relay TURN over TLS on 443" (checked when
  this lands) and "Gateway relays over TURN TCP/TLS (webrtc-rs async
  relayer)".

## Verification

`scripts/check-relay.sh`, run by `make check-relay`. Written first and run
against today's configuration, where it must fail (nothing listens on 443).
Against the real compose file, in order:

1. Refuse to run if anything listens on 443 or 3478.
2. Generate self-signed certificate A in a temporary lineage directory and
   install it with `install-cert.sh` (`CERT_GROUP` = caller's group). No relay
   is running: the hook must say so and exit 0.
3. Start the relay (`RTC_TURN_REALM=relay.test`, `RTC_TURN_PUBLIC_IP=127.0.0.1`,
   a random secret, `RTC_TURN_CERT_DIR` = a temporary directory,
   `RTC_TURN_CERT_GROUP` = caller's group) and wait up to
   60 s for `healthy`.
4. A TLS handshake on 443 presents certificate A (serial compared).
5. `turnutils_uclient -S -t -p 443` with the secret shows the channel-bind 403
   (Forbidden IP) and no allocate or authentication error.
6. `install-cert.sh` with a key that does not match its certificate exits
   non-zero, and 443 still presents A.
7. `install-cert.sh` with certificate B: within 10 s, 443 presents B. No
   restart — this covers copy, permissions and the signal.
8. Replace the installed key with one mode 0600, restart the relay: within
   60 s it is `unhealthy` (about 40 s with the healthcheck timings).
9. A trap stops the relay and removes temporary files whatever happened.

## Out of scope

The gateway side (part two). An ACME container. Serving TLS and HTTPS on the
same 443. DTLS TURN (coturn opens it on UDP 443; no client here uses it, and
the firewall rule stays TCP-only). Changing `turn.rs` or the browser. Deploying
a real relay — nothing in the repository names a real relay host, so real-world
verification is recorded as not done.
