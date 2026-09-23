# Recording policies: a ring on the gateway, and what is worth keeping

**Goal:** recording is one command at a time. The dashboard asks for thirty
seconds, the gateway dials the camera, records, uploads, and that is the whole
feature. Nobody can say "record this entrance during working hours", nobody
can save what already happened, and a camera that goes dark takes the minutes
before it went dark with it. This makes recording a *policy*, and makes the
minutes before an event recoverable.

**Decided with the user (2026-09-23):**

- **A ring buffer on the gateway.** It writes continuously to its own disk
  within a byte budget and prunes the oldest; only what something asks to keep
  is uploaded. Uploading everything would cost about 650 GB a month per
  2 Mbit/s stream whether or not anyone ever watches it — the same argument
  that put ingest on the gateway in `docs/VIDEO-SOURCES.md`.
- **Four things can keep a clip:** a schedule, an operator pressing *save the
  last N minutes*, an AI plugin's result crossing a threshold, and a source
  incident.

## What exists today

- `archive::record_from` records N seconds off a `FrameSource` into fMP4: one
  `init.mp4` and media segments cut on keyframes.
- The API queues a `Record` command; the gateway runs it, asks for signed PUT
  URLs, uploads past the API, and registers a manifest. `docs/ARCHIVE.md`.
- `GATEWAY_STATE_DIR` holds the identity and camera credentials, 0700, and is
  the only directory the hardened unit may write.
- Sources are polled as a list (`GET /api/v1/gateways/{id}/sources`), which is
  the shape a policy list should copy.
- Incidents exist in the control plane. The gateway knows a source went
  offline before the cloud does, because it is the thing that noticed.

## Design

### The ring

Per camera, under `RECORDING_DIR` (default `GATEWAY_STATE_DIR/ring`):

```
ring/<camera_id>/init-<hash-of-the-init-bytes>.mp4
ring/<camera_id>/<start-unix-ms>-<duration-ms>-<init-hash>.m4s
```

**The directory is the index**, and there is no index file: a name carries the
segment's start, its length and which init decodes it, so a gateway killed
mid-write recovers by reading the directory rather than by trusting a file it
may not have finished. Segments are written to a `.partial` name and renamed,
so a reader never sees half of one. They are the recorder's existing fMP4
output, so a kept clip is an init and a contiguous run of segments — no
re-muxing.

A parameter change starts a new init segment and ends any clip that spans it,
because a recording that changes shape halfway is not one recording.

**Budget.** `RECORDING_BUDGET_BYTES` for the whole ring, divided between the
cameras that have a continuous policy. Over budget, the oldest segment goes
first. The budget is a promise about disk, not about hours: how many hours it
buys depends on the bitrate, and the dashboard says which it is getting.

**Cost of running it.** A continuous ring means one RTSP session per camera,
always. That is the operator's choice per camera, and the default is off.
A pushed source costs nothing extra: it is already arriving.

### Policies

Control plane, one per camera, polled beside the source list:

```rust
pub struct RecordingPolicy {
    pub camera_id: String,
    pub gateway_id: String,
    pub mode: RecordingMode,        // Off | Continuous
    pub keep: Vec<KeepRule>,
    pub retention_days: u16,        // how long a kept clip lives in the cloud
}

pub enum KeepRule {
    Schedule { days: WeekMask, from: Time, to: Time },
    OnIncident { pre_roll_seconds: u16, post_roll_seconds: u16 },
    OnAnalysis { plugin_id: String, every_seconds: u16, threshold: f32,
                 pre_roll_seconds: u16, post_roll_seconds: u16 },
}
```

`GET /api/v1/gateways/{id}/recording-policies` behind the gateway bearer;
`POST /api/v1/cameras/{id}/recording-policy` from the dashboard, with audit
rows. A list, not a command, for the same reason sources are.

### Keeping a clip

One path, whatever asked:

1. Something names a range: `(camera_id, from, to, reason)`.
2. The gateway picks the segments covering it, widening to the keyframe at or
   before `from`. Asking for more than the ring kept is not an error — "save
   the last hour" on a ring holding twenty minutes hands over the twenty, and
   the manifest says which range it actually covers. A range the ring holds
   *nothing* of is an error that says what it does hold.
3. The init segment and those media segments upload through the existing
   storage plugin path, and the manifest is registered as today, with
   `reason` and the requested range on it.

The four callers:

- **Schedule** — the gateway's own clock, keeping each window as it closes.
- **Save the last N minutes** — a new command from the dashboard. The one
  trigger that only a ring can serve, and the reason for the whole design.
- **Analysis** — the gateway pulls a frame every `every_seconds`, calls the AI
  plugin it was named, and keeps a window when the score crosses the
  threshold. This is the only trigger that needs something that does not exist
  yet (a frame scheduler), so it comes last.
- **Incident** — the gateway already knows when a source stops answering, and
  the minutes before that are what an investigation wants.

### Retention

`retention_days` is enforced where the clip lives: the control plane knows
every manifest and can delete through the storage plugin. The ring's own
retention is the byte budget, and nothing else.

## Slices

1. **The ring.** Continuous segmenter to disk, budget, pruning, recovery by
   reading the directory. Gateway only, nothing uploads.
2. **Keep a range.** Clip assembly from the ring, upload, manifest, and the
   *save the last N minutes* command end to end.
3. **Policies.** API model, dashboard, gateway polling, schedule-driven keeps.
4. **Incident-driven keeps**, with pre-roll.
5. **Analysis-driven keeps**, which brings the frame scheduler with it.
6. **Docs**: `docs/RECORDING.md`, the backlog line, and the honest note about
   what has run on real hardware.

## Testing

TDD as always.

- Ring: segments land with their range in the name and a second `Ring` over
  the same directory sees them; the budget prunes oldest-first and keeps the
  newest; an init goes when its last segment does; a parameter change starts
  a new init **and keeps the video either side of it**; a camera id cannot
  name a directory outside the ring.
- Keep: a range inside the ring produces a clip with the init that decodes it;
  a range the ring holds nothing of says what it does hold; a range longer
  than the ring comes back short rather than refused; a range spanning a
  parameter change is refused, and either side of it is not.
- Policies: the list is polled and survives an API that is not answering; a
  schedule window keeps exactly one clip; turning a policy off stops the ring
  and frees its disk.
- Incident: a source that stops answering leaves a clip that ends at the
  silence and starts `pre_roll` before it.
- Web: the policy editor, and *save the last N minutes*.

## Out of scope

Motion detection of our own — analysis is a plugin, as everywhere else.
Transcoding, still. Audio. Editing or trimming a clip after the fact.
Continuous *upload*, which is the thing this design exists to avoid.
