# Recording: what is kept, and what it costs

Recording used to be one command at a time: the dashboard asked for thirty
seconds, the gateway dialled the camera and uploaded the result. Nobody could
say "record this entrance during working hours", nobody could save what had
already happened, and a camera that went dark took the minutes before it went
dark with it.

A camera now has a *recording policy*, and a gateway with one keeps a ring
buffer on its own disk. Only what a policy, an operator, a plugin or an
incident asks for is ever uploaded.

## The two halves

**The ring** is on the gateway. A camera set to record continuously is
recorded the whole time into fMP4 segments under `RECORDING_DIR`, inside a
byte budget that drops the oldest when it is full. Nothing leaves the site.

**A kept clip** is a stretch of that ring uploaded through the storage plugin
and filed in the archive, where it plays like any other recording.

That split is the whole design. Uploading everything would cost roughly
650 GB a month per 2 Mbit/s stream, per camera, whether or not anyone ever
watches it. Keeping it on the gateway costs a disk.

## Setting a policy

*Recording* on each camera's card in the dashboard:

| | |
| --- | --- |
| **Only when asked** | the default: nothing is recorded until a command asks |
| **Continuously** | the gateway keeps a ring for this camera |
| **Keep from … until …** | a window of the day to upload, in the site's own time |
| the day checkboxes | which days that window applies to; all of them means every day |
| **Keep for (days)** | how long a kept clip lives in the cloud |

Continuous with no window fills the ring and keeps nothing on its own, which
is what you want for a camera you only ever clip by hand.

The gateway polls its policies beside its source list. A list is state rather
than an event: a restart loses nothing, a removal arrives on its own, and an
API that cannot be reached leaves a site recording exactly as it was.

## The four ways video gets kept

- **A schedule.** The window is cut into clips of at most ten minutes once it
  has passed. A window still running keeps its tail back — cutting whatever
  has piled up on every poll would turn a morning into a hundred clips the
  length of the poll interval. Overlapping rules are merged, so the same
  minute is never uploaded twice.
- **Save the last five minutes**, on the camera's card. The one thing only a
  ring can do. Asking for more than the ring kept is not an error: you get
  what it has, and the recording says which range it really covers.
- **An incident.** A camera that stops answering leaves the minutes before the
  silence behind, and the first minutes after it comes back. The gateway is
  what notices — it is the one dialling — so this happens before the cloud
  knows anything is wrong. A camera answering *badly* is not an incident:
  packet loss is a different conversation.
- **A plugin.** "Ask this plugin every thirty seconds, and when it is sure
  enough about something, keep the video around that moment." The gateway has
  no analysis of its own and is not getting any. The snapshot it takes is not
  stored: a camera looked at every thirty seconds is two and a half thousand
  images a day, and the point of looking is the clip.

## What it costs a site box

| | |
| --- | --- |
| `RECORDING_DIR` | where the ring lives; defaults to `ring/` inside the state directory |
| `RECORDING_BUDGET_BYTES` | disk per camera, default 2 GiB |

A continuous policy means one RTSP session per camera, always — the recorder's
own, beside the probe and any live view. A pushed source (RTMP or SRT) costs
nothing extra: it is already arriving.

The budget is a promise about disk, not about hours. How many hours it buys
depends on the bitrate: 2 GiB is about two and a half hours of a 2 Mbit/s
stream, and about twenty minutes of a 15 Mbit/s one. The journal says how much
each camera's ring actually holds every time a recording session ends.

## How a clip is cut

Segments start on keyframes, so a clip asked for mid-segment widens backwards
to the keyframe before it: a clip that began mid-GOP would not decode. A clip
may not span a change of resolution — no player would sit through that as one
file — and the error says to ask either side of it.

The directory is the index. Each segment's name carries its start, its length
and which init segment decodes it, so a gateway killed mid-write recovers by
reading the directory rather than trusting a file it may not have finished.
Segments are renamed into place, so a reader never sees half of one.

## What has met real hardware

- **The ring and the clipping** have run against the fake camera and a real
  transport stream in the test suite, and against no site. The recorder
  underneath them is the one a Dahua NVR proved over twenty hours
  (`docs/HARDWARE-NOTES.md`), and the segmenting is shared with it.
- **Schedules** are tested against a clock that does not move, including
  windows crossing midnight and a site three hours east of UTC. They have
  never run across a daylight-saving change on a real box.
- **Incident keeps** are tested by unplugging a camera mid-recording.
- **Plugin-driven keeps** have run against a plugin that always sees a person.
  No real detector has ever driven one.
- **Nothing has filled a disk yet.** The budget is exercised with megabytes in
  tests, not with a fortnight of a real camera, and pruning under a genuinely
  full filesystem — as opposed to a full budget — is untested.
