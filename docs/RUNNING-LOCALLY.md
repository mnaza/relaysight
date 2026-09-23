# Running the whole thing on one machine

Verified end to end on 2026-09-04 against a real Dahua camera reached over the
internet. Every command below was run, not written from memory.

## The short version

```bash
docker compose up -d --build api web     # dashboard on :8081, API on :8080
```

Open `http://localhost:8081/`. That is the cloud half. It has no cameras yet.

## A camera the gateway can actually reach

If your cameras are on the same network segment as the gateway, ONVIF discovery
finds them and there is nothing to configure. If they are not — a different
VLAN, a VPN, a port forward — discovery is multicast and will never see them.
Two ways round it:

**ONVIF over a routed path**, when the camera's ONVIF port is reachable:

```bash
export ONVIF_DISCOVERY_SECONDS=0
export ONVIF_HOSTS=192.168.1.50
export CAMERA_USERNAME=admin CAMERA_PASSWORD='...'
make edge
```

**Cameras with their own passwords.** `CAMERA_USERNAME` and `CAMERA_PASSWORD`
are the fallback every camera without an entry of its own uses. Where a camera
has its own, put it in the gateway's credential store instead:

```bash
printf '%s\n' 'the-camera-password' |
  vms-gateway credentials set 192.168.1.50 admin
vms-gateway credentials list      # hosts and usernames, never a password
vms-gateway credentials remove 192.168.1.50
```

The password is read from stdin, so it never lands in `ps` or a shell history.
Entries live in `GATEWAY_STATE_DIR/camera-credentials.enc`, encrypted with the
same key as the gateway's identity and written 0600; a file that will not
decrypt stops the gateway rather than letting every camera quietly fall back to
the shared pair. Keyed by address as written in `ONVIF_HOSTS` — the same camera
answers ONVIF on one port and RTSP on another, and one entry covers both. An
IPv6 camera keeps its brackets, `[2001:db8::1]`, in both places.

**Raw RTSP**, when only the stream port is reachable. This skips ONVIF entirely,
so there is no profile selection, no substream for live view and no snapshots:

```bash
export ONVIF_DISCOVERY_SECONDS=0
export CAMERA_RTSP_URL='rtsp://user:pass@host:554/path'
export CAMERA_NAME='Front door'
make edge
```

The second is what a camera behind a single forwarded RTSP port needs, and it is
what was used to verify this document.

Some Dahua firmware writes the SSRC as decimal in `RTP-Info` where retina
expects hex, and stock retina ends the session at PLAY with `Unparseable ssrc`.
The gateway builds against a fork that accepts it — `edge/gateway/Cargo.toml`
pins it by commit until scottlamb/retina#137 lands — so the image and a
`cargo run` both handle that camera, with nothing to patch locally.

## Logging in

`ADMIN_PASSWORD` seeds the single admin account the first time the API boots
against an empty store, and is ignored on every boot after that — change the
password from the avatar menu in the dashboard, not by editing the env var.
Set `ADMIN_PASSWORD_RESET=true` to force a re-seed from `ADMIN_PASSWORD` on the
next boot instead; that also signs out every existing session. The flag applies
on **every** boot while it is set, not just the next one — unset it once you are
back in, or each restart will silently wipe all sessions again. The password
must be at least 12 characters; a shorter one is refused at seed time the same
way it is refused in the dashboard. For an HTTPS
deployment set `AUTH_COOKIE_SECURE=true` so the session cookie is marked
`Secure`. There is no way to run the API with no credential at all — it
refuses to start rather than boot a dashboard nobody can log into.

## Where the database lives

The API keeps fleet identity, gateway tokens, enrollments and recording
manifests in one SQLite file. Run bare (`cargo run -p vms-api`) it is
`data/vms.db` next to where you started the binary; under compose it is
`/data/vms.db` on the `api-data` named volume, so `docker compose down` and
restarts keep enrolled gateways enrolled. `DATABASE_URL=sqlite:<path>` moves it.

Backup is copying that one file while the API is stopped.

## Incidents

Once a minute the API reconciles every camera and records an incident when one
goes offline — either the gateway reported it, or it has gone silent past
`STALE_CAMERA_SECONDS` (default 75) — and closes the incident when the camera
is seen again. History is kept for `INCIDENT_RETENTION_DAYS` (default 90; `0`
means forever); an incident that is still open is never pruned, regardless of
age. There are no incidents for the first stale-window after the API starts —
by design, since it has not watched any camera long enough yet to call silence
unusual.

## Gateway identity

After a successful enrollment the gateway writes its identity — token,
customer/site, camera limit — to `GATEWAY_STATE_DIR` (default `data/gateway`;
`/data` under compose, on the `gateway-data` volume). It's encrypted with a
key file generated beside it, or with `GATEWAY_STATE_KEY` (64 hex characters)
when set. State without its key is useless, which is what the encryption
buys: safe backups and copies, not protection from root.

On later boots the persisted identity wins and `ENROLLMENT_TOKEN` is ignored.
`GATEWAY_REENROLL=true` plus a fresh enrollment token wipes and re-enrolls —
also the fix for undecryptable state, which otherwise refuses to start.

## Revocation and the audit log

Revoke a gateway from the gateways grid in the dashboard, or directly with
`POST /api/v1/gateways/<id>/revoke`. Revoking an id that doesn't exist is a
404, not a silent success.

A revoked gateway is refused every credential it could present, including the
shared bootstrap token — revocation evicts, it doesn't just invalidate that
gateway's own per-gateway token. The un-revoke is a fresh enrollment: running
the enroll flow again for that gateway id clears the tombstone and restores
access, the same as a first-time enroll.

A revoke is deliberate and lands in the audit log, so it is not an outage: the
revoke closes any open incident on that gateway's cameras, and the incident
pass stops tracking them for as long as the gateway stays revoked. The cameras
stay in the fleet roster, shown offline. Re-enrolling the gateway brings its
cameras back under watch. One wrinkle there: a camera that is still dark after
the re-enroll gets an incident dated from its last report before the revoke, so
the duration includes the time it was revoked.

When the gateway is not coming back, *Retire cameras* on its card — or
`POST /api/v1/gateways/<id>/cameras/retire` — takes its cameras out of the
roster and answers `{"retired": <count>}`. It refuses with a 409 unless the
gateway is revoked, so a working site cannot be emptied by a misclick. Nothing
is deleted: the cameras are stamped `retired_at`, their recordings still play
back, and a gateway reporting one of them again brings it back with the history
it had. Run twice, the second answers `{"retired": 0}`. The row it leaves is
`cameras.retired`.

A gateway that has only ever reported with the shared bootstrap `GATEWAY_TOKEN`
(never enrolled) has no roster row to revoke — the revoke endpoint answers 404
for it; the remediation there is rotating `GATEWAY_TOKEN`.

The known limit: the two plugin endpoints that carry no gateway id in their
path (`/api/v1/plugins/{plugin_id}/ai/analyze` and
`/api/v1/plugins/{plugin_id}/storage/uploads`) answer to the bootstrap secret,
or to any enrolled gateway's own token, regardless of revocation — there's no
gateway id in the request for the check to key off. This is the same trust
that secret already carries elsewhere; it isn't a new hole, but a revoked
gateway isn't locked out of these two specifically.

`GET /api/v1/audit` lists security events: logins (`login.ok`, `login.failed`),
password changes and resets (`password.changed`, `password.change.failed`,
`password.reset`), enrollments (`enrollment.created`, `gateway.enrolled`), and
revocations (`gateway.revoked`, `cameras.retired`). Entries are kept forever by default; set
`AUDIT_RETENTION_DAYS` to a positive number of days to prune older ones.

The two failure rows carry how many wrong passwords have come in a row — both
paths share one throttle, so a burst is visible in the log and not only in how
long the answer took.

## Checking it without a browser

```bash
curl -s -H "Authorization: Bearer demo-local-token" \
  localhost:8080/api/v1/cameras | python3 -m json.tool
```

A working camera looks like this. Note the endpoint: the password is stripped
before anything leaves the gateway.

```json
{"name": "Dahua channel 1", "status": "warning", "codec": "h264",
 "fps": 4.39, "bitrate_kbps": 1899, "packet_loss": 573,
 "rtsp_endpoint": "rtsp://198.51.100.20:55544/cam/realmonitor?channel=1&subtype=0"}
```

`status: warning` there is honest. That camera was reached across the public
internet and the link could not carry the main stream, so the encoder dropped
frames — 4.4 fps against 25, and 573 lost packets. On a LAN the same camera runs
clean. The telemetry saying so is the point.

## Adding the relay

```bash
RTC_TURN_PUBLIC_IP=127.0.0.1 RTC_TURN_REALM=localhost \
RTC_TURN_SECRET=demo-turn-secret \
  docker compose -f deploy/coturn/docker-compose.yml up -d
```

Give the API the same secret with `RTC_TURN_URLS` and `RTC_TURN_SECRET`, and
`/api/v1/rtc/config` starts returning a relay alongside the STUN server:

```json
{"urls": ["turn:127.0.0.1:3478?transport=udp"],
 "username": "1788521098:browser", "credential": "..."}
```

That username is an expiry and the credential is an HMAC of it. Taking that
exact pair from the API and running `turnutils_uclient` against the relay
allocates successfully, which is the join between the two halves and is now
checked rather than assumed. See `docs/TURN-DEPLOY.md`, including what is
verified and what is not.

`make check-relay` runs the relay's own compose file with throwaway certificates
and checks TURN over TLS on 443 end to end: the certificate hook, an
authenticated allocation, renewal without a restart, and the healthcheck. It
needs Docker, openssl, ss, and 443 and 3478 free, and refuses to start
otherwise.

`make check-gateway-relay` checks the gateway's side: it starts its own coturn
in Docker on high ports with a throwaway certificate authority, points the
gateway's UDP TURN URL at a closed port, and runs a live session that can only
reach the browser through the TLS bridge. It needs Docker, openssl, `ss`, and
ports 13478, 13479 and 15349 free.

`make check-installer` boots Debian with systemd in a privileged container and
runs the real `deploy/gateway/install.sh` against a local release channel signed
with a throwaway key. It covers install and enrolment, an update, a rollback, a
wrong-key refusal, the credentials wrapper and a re-run. It needs Docker,
python3, openssl, and ports 18090 and 18091 free. What it proves and what it
does not is in `docs/INSTALL-GATEWAY.md`.

`make check-ingest` has ffmpeg publish the committed H.264 fixture into the
gateway's own listeners — RTMP, and SRT with the `srt` feature — copying the
bytes rather than re-encoding, so the framing and the timing are ffmpeg's. It
needs nothing but ffmpeg with both protocols compiled in. Every other ingest
test uses a double this repository wrote, and doubles agree with the code that
expects them.

## If the ports are taken

`docker-compose.override.yml` is gitignored, so a machine with something else on
8080 can move them without touching the committed file:

```yaml
services:
  api:
    ports: !override
      - "8090:8080"
  web:
    ports: !override
      - "8091:80"
```

The gateway then needs `API_URL=http://127.0.0.1:8090`. Nothing else changes;
the dashboard talks to the API through nginx inside the compose network.
