# Video sources that are not cameras

**Goal:** a gateway only finds cameras — ONVIF discovery, or one RTSP URL in
`CAMERA_RTSP_URL`. A customer who already has an NVR, an encoder, a drone or
another VMS has video we cannot take, and adding it means editing the
gateway's environment by hand. This makes a source something added from the
dashboard: an address the gateway pulls, or a stream pushed to the gateway.

**Decided with the user (2026-09-22):**

- **The gateway polls a list, not a command.** `GET
  /api/v1/gateways/{id}/sources` beside the command poll. A list is state,
  not an event: a restart loses nothing, a repeat is harmless, and a removal
  arrives on its own.
- **Passwords live only on the gateway.** The dashboard takes a URL without
  credentials; the password goes in on the box with
  `relaysight-gateway-credentials`, keyed by address, as camera credentials
  already are. The control plane never holds a source's password.
- **Push ingest (SRT/RTMP) is in scope**, not only pulled RTSP.

**Sequencing, and why.** `live.rs`, `archive.rs`, `snapshot.rs` and `rtsp.rs`
each open their own retina session; there is no frame-source abstraction to
implement for a new protocol. So push ingest is not "one more parser" — it
needs that abstraction first, across the most delicate code in the gateway.
The order below keeps each step shippable: the product feature lands first
against RTSP, which needs no new media path, and the refactor arrives on its
own before any new protocol.

## Design

### A source

```rust
pub struct VideoSource {
    pub id: String,            // uuid
    pub gateway_id: String,    // the gateway that carries it
    pub name: String,
    pub kind: SourceKind,      // Rtsp | Rtmp | Srt
    pub address: String,       // rtsp://host/path, or a stream key for push
    pub added_at: DateTime<Utc>,
}
```

Stored in the API (`0006_video_sources.sql`), managed from the dashboard,
never carrying a credential. A camera id is derived from the address exactly
as the explicit-URL path already derives it (`Uuid::new_v5` over the URL), so
a source keeps its identity, its recordings and its incidents across
restarts and re-adds.

### The API

- `POST /api/v1/sources` — add (session-protected), `GET /api/v1/sources`,
  `DELETE /api/v1/sources/{id}`. Adding validates the address per kind and
  refuses userinfo in a URL: that is where a password would hide.
- `GET /api/v1/gateways/{id}/sources` — the gateway's own list, behind the
  gateway bearer, same as the command poll.
- Audit rows: `source.added`, `source.removed`.

### The gateway

A poll beside the command poll, on the same interval, merged into the probe
pass: discovered cameras first, then sources, skipping any whose address a
discovered camera already covers. Everything downstream — telemetry, live,
recording, plugins — sees a camera like any other, because `CameraSource` is
already just an address with optional credentials. Credentials come from the
existing per-camera store, keyed by the address's host.

`CAMERA_RTSP_URL` stays, as the way to bring a gateway up before it can reach
the API.

### Frames, once, for every protocol

Today each media path opens its own retina session. A new protocol would have
to be added to each. So:

```rust
pub trait FrameSource: Send {
    /// The next H.264 access unit, or None when the stream ended.
    async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>>;
    fn parameters(&self) -> Option<VideoParameters>;
}
```

`retina` becomes one implementation, `RtspSource`, and framing is chosen when
a source is opened rather than converted afterwards: live view asks for Annex
B, the recorder for four-byte lengths. So each consumer's bytes are the bytes
it already had, which is what its existing tests pin.

`live.rs` and `archive.rs` split in two: the part that opens RTSP, and
`pump_from`/`record_from` over `&mut dyn FrameSource`. `snapshot.rs` fetches
over HTTP and never opened a session. The probe in `rtsp.rs` keeps its own:
it counts lost RTP packets and reports whatever codec the SDP named, neither
of which is a property of an access unit. Only then does a second
implementation make sense.

### Push ingest

A pushed stream arrives at the gateway rather than being fetched:

- **RTMP** — a listener wherever `RTMP_LISTEN` says (off when unset), with
  `rml_rtmp` doing the handshake and the chunk stream. A publisher is accepted
  only on a stream key the dashboard listed as a source; anything else is
  refused with `NetStream.Publish.Denied`, because the port may be reachable
  from the camera network. FLV video tags carry H.264 in AVCC, which is what
  the recorder already writes, so nothing is converted. Timestamps are
  milliseconds, which is the stream's clock rate. Width and height come from
  the publisher's `onMetaData`; without it live view still works and the
  recorder says what it is missing. Audio tags and composition time offsets
  (B-frames) are dropped.
- **SRT** — a listener via `srt-tokio` on `SRT_LISTEN`, behind the `srt`
  feature. The caller's stream id names the source, either bare or as
  `#!::r=<key>`, and only listed keys are accepted. SRT carries MPEG-TS, so
  `srt::ts` follows the PAT and PMT to the first H.264 stream and reassembles
  its PES packets; the parameter sets arrive in band, which is why `h264`
  reads the picture size out of the SPS. Built rather than taken from a crate
  because the maintained TS readers are push-style filter machinery and this
  needs one stream in one direction; the test vector is a transport stream
  ffmpeg produced. Behind the flag until it has met a real encoder: what it
  has met is an SRT caller in this process.

Both are local listeners: the video never touches our servers, so the
economics do not change. Ports and stream keys are gateway settings, not
control-plane state.

## Money, since it decides the shape

A pulled or pushed source costs the gateway's bandwidth on site and our
storage only for what is recorded — the same as a camera. The moment the
*cloud* pulls a stream instead, we pay ingest, storage and egress for every
stream continuously: about 650 GB a month for one 2 Mbit/s feed. So: the
gateway pulls, always. Cloud ingest is deliberately not in this design.

## Testing

TDD as always.

- API: adding, listing and removing a source; a URL carrying userinfo is
  refused; the gateway's list is behind the gateway bearer and contains only
  its own; audit rows appear.
- Gateway: the poll merges sources with discovered cameras; a source and a
  camera at one address do not appear twice; a removed source disappears
  from telemetry; credentials come from the store by host.
- Frame source: the retina implementation passes the existing live, archive
  and probe tests unchanged (that is the point of the refactor).
- RTMP: a handshake and a published stream produce H.264 frames — tested with
  an in-process publisher, no external tools.
- SRT: the same, behind its feature flag.
- Web: adding a source from the dashboard, and the list.

## Out of scope

Cloud-side ingest. Transcoding — every path stays H.264 passthrough, and a
source that is not H.264 is refused with a message that says so. Pulling from
consumer platforms (YouTube, Twitch): their terms forbid it, and that is a
product decision, not a technical one. Audio.
