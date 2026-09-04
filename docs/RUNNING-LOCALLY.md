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

⚠️ **A Dahua needs the retina patch.** Some firmware writes the SSRC as decimal
in `RTP-Info` where retina expects hex, and the session dies at PLAY with
`Unparseable ssrc`. Until scottlamb/retina#137 lands, add to `Cargo.toml`:

```toml
[patch.crates-io]
retina = { path = "../retina-fork" }
```

and run the gateway with `cargo run -p vms-gateway` rather than from the image,
since the image is built without it.

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
checked rather than assumed. See `docs/TURN-DEPLOY.md`, including the part that
does not work yet.

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
