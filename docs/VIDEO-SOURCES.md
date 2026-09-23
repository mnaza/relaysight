# Video that is not a camera the gateway found

A gateway discovers cameras by ONVIF, and that covers a site where the
cameras are on the same network as the box. It does not cover the customer
who already has an NVR, an encoder, a drone link or another VMS. A *video
source* is that video: something added from the dashboard, carried by a
named gateway, and treated as a camera by everything downstream — telemetry,
live view, recording, plugins, incidents.

## What can be a source

| Kind | The address is | Who starts the connection |
| --- | --- | --- |
| **RTSP** | `rtsp://host/path` | the gateway dials it |
| **RTMP** | a stream key | the publisher pushes to the gateway |
| **SRT** | a stream key | the publisher pushes to the gateway |

RTSP is the ordinary case: an NVR's sub-stream, a camera on a routed network
discovery cannot reach, another VMS's re-stream. RTMP and SRT are for video
that cannot be dialled — an encoder behind NAT, a link that only knows how to
publish.

Every path is H.264 passthrough. A source that is not H.264 is refused with a
message that says so, because transcoding on a site box is how a gateway
stops keeping up with its cameras.

## Adding one

*Sources* on the dashboard: a name, the gateway that carries it, the kind and
the address. The gateway picks it up on its next poll — it asks
`GET /api/v1/gateways/{id}/sources`, and a list is state rather than an
event, so a restart loses nothing and a removal arrives on its own. A gateway
that cannot reach the API keeps carrying what it was last told.

A source keeps its identity across restarts and re-adds: its camera id is
derived from its address, so recordings and incidents stay attached to it.

## Passwords

**A source's password never goes to the control plane.** An address carrying
credentials is refused — `rtsp://admin:pw@host/path` comes back with a
message saying where the password belongs, which is on the gateway:

```bash
printf '%s\n' 'the-password' \
  | sudo relaysight-gateway-credentials set 10.0.0.7 admin
sudo systemctl restart relaysight-gateway
```

Keyed by host, so every source at that address uses it. See
`docs/INSTALL-GATEWAY.md`.

## Being published to

A pushed source is a stream key, and the gateway has to be listening. Neither
listener is on unless it is told where to bind, and that is set the way every
other gateway setting is: re-run the installer with `--env`, which keeps the
rest of the configuration, or edit `/etc/relaysight/gateway.env` and restart.

```bash
curl -fsSL https://.../install.sh | sudo sh -s -- --env RTMP_LISTEN=0.0.0.0:1935
```

| | |
| --- | --- |
| `RTMP_LISTEN` | `host:port` for RTMP, conventionally 1935 |
| `SRT_LISTEN` | `host:port` for SRT, conventionally 9000 |

SRT is compiled out unless the gateway was built with `--features srt`, and
the released binaries are not, so `SRT_LISTEN` does nothing on an installed
gateway today.

The dashboard shows each pushed source's publish URL. An encoder publishes to
`rtmp://<gateway>:1935/live/<key>`, or over SRT with the stream id `<key>` —
`srt://<gateway>:9000?streamid=<key>`, and `#!::r=<key>` works too, which is
what some encoders write.

**Only keys the dashboard listed are accepted.** The port is reachable from
the camera network, and often from further; a publisher on an unregistered
key is refused by name rather than quietly recorded. Removing a source stops
the next publisher using its key.

## What is refused, and why

- **Credentials in an address** — they would then live in the control plane's
  database and its audit log. The gateway is where they belong.
- **Anything that is not H.264** — no transcoding, anywhere in this system.
- **A stream key nobody registered** — see above.
- **A scrambled transport stream** — nothing here can decrypt it, and writing
  the bytes anyway would put noise in a recording.
- **Consumer platforms — YouTube, Twitch and the like.** Their terms forbid
  pulling streams this way. That is a product decision, not a missing
  feature, and no amount of code changes it.

## Why the gateway pulls, and not the cloud

It would be less work to have our servers pull a URL and hand it on. It would
also change what this costs. A stream pulled into the cloud is ingest,
storage and egress that we pay for continuously — about 650 GB a month for a
single 2 Mbit/s feed, per feed, whether or not anyone is watching. Pulled or
pushed on the gateway, a source costs the site's own bandwidth and costs us
storage only for what is recorded, exactly as a camera does.

So the gateway pulls, always, and the listeners are on the gateway. Cloud
ingest is deliberately not built.

## What has met real hardware

The distinction this project keeps everywhere:

- **RTSP sources** run the same code path as a discovered camera, which has
  been proven against a Dahua NVR over the internet for more than twenty
  hours. See `docs/HARDWARE-NOTES.md`.
- **RTMP ingest** has never met a real encoder. It is tested against the real
  client half of `rml_rtmp` over a real socket — handshake, chunk stream, AMF
  commands, FLV video tags — and a stream published that way comes out as an
  fMP4 recording. What is untested is what OBS, ffmpeg and a hardware encoder
  do differently from that.
- **SRT ingest** is behind the `srt` feature for the same reason, one step
  further back: it has met an SRT caller in the test process and a transport
  stream ffmpeg produced, and no encoder at all. The transport-stream reader
  is ours rather than a crate's, which is worth knowing when the first real
  stream behaves oddly.
- **Audio** is carried by none of them.

The first person to point any of these at real equipment should expect to
find something. That has been true every time so far.
