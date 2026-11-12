# A camera that flaps: one message out, one when it settles

**Goal:** a camera that goes down and comes back every minute raises an event
every minute, and every one of them leaves the building. Thirty "stopped
answering" and "is answering again" messages in an evening teach the person
in the channel to mute the channel, and then the real outage goes unread.
Instead, the person sees one message saying the camera is flapping, and one
more when it has stopped, saying which way it ended up.

**Decided with the user (2026-11-10):**

- One message out when the flapping is noticed, one when it has settled.
- The dashboard keeps showing every transition. The collapsing happens only on
  the way out: what is stored does not change, what is *delivered* does.
- The two new events are ordinary events: stored, shown in the dashboard as
  rows among the others, delivered through the same outbox.

## What exists today

- `incident_pass` (`services/api/src/main.rs`) runs once a minute. For every
  camera it opens or closes an incident; only a transition (`Ok(true)` from
  `open_incident` / `close_incident`) raises a `FleetEvent` —
  `camera_offline` (critical) or `camera_recovered` (info) — through
  `raise_event`, which writes the event and queues one delivery row per
  registered `event_sink` (`record_event`). Severity and title are what a sink
  posts; the reference webhook sink posts `title` and `detail` and nothing else.
- Incidents live in `incidents(camera_id, started_at, ended_at, detail)`, one
  open per camera by index. Every transition is therefore already on disk as
  an incident starting or ending.
- The dashboard's *Alerts* panel lists events with their delivery state; an
  event with no delivery rows reads "no event_sink plugin registered".
- `docs/superpowers/specs/2026-09-23-alerts-design.md` put deduplication out
  of scope: "that belongs in a rule engine that does not exist yet". This is
  not a rule engine. It is one rule, for the one case that makes people mute
  alerts, and it lives where the events are raised.

## Design

### The two new events

| kind | severity | when |
| --- | --- | --- |
| `camera_flapping` | warning | the transition that crosses the threshold |
| `camera_settled` | info if the camera ended up answering, **critical if it ended up down** | the first pass after the camera has held one state for the settle window |

Titles, in the voice of the existing ones:

- `Yard camera is flapping at Bakery` — detail: `3 changes in 10 minutes;
  single changes are held until it settles`.
- `Yard camera settled and is answering at Bakery` — detail: `7 changes;
  quiet since 21:52 UTC`. (The first draft said "7 changes over 38 minutes";
  review found that minutes figure could only be a convention, since the
  row does not know when the first counted change was, and a reader would
  take it for the measured span. Both figures that ship are measured.)
- `Yard camera settled and is still down at Bakery` — detail: `7 changes;
  down since 21:52 UTC`. Critical, because "settled" must not read
  as good news when the camera is off the air: this event *is* the outage
  alert for that camera.

Both carry `camera_id`, `gateway_id`, and `metadata.changes` (the count),
`metadata.flapping_since`; the settled one also `metadata.state`
(`"up"`/`"down"`).

### Detecting a flap

At a transition in `incident_pass`, count this camera's transitions inside the
window: incidents with `started_at` in the window plus incidents with
`ended_at` in the window (the one just opened or closed included), **and after
the camera's last settle** — a transition that was already reported as part of
a flap that ended does not count toward the next one. Without that, a settle
window shorter than the flap window would re-cross the threshold on the first
transition after "settled" and the channel would get a second "is flapping"
for the same transitions (my call, from the builder check). When the count
reaches the threshold and the camera is not already flapping, the camera
*starts flapping*: its `camera_flaps` row is written, or rewritten, and
`camera_flapping` is raised.

Defaults, each an environment setting in the `STALE_CAMERA_SECONDS` style:

| setting | default | meaning |
| --- | --- | --- |
| `FLAP_TRANSITIONS` | 3 | transitions inside the window that make a flap |
| `FLAP_WINDOW_SECONDS` | 600 | the window |
| `FLAP_SETTLE_SECONDS` | 600 | one state held this long ends the flap |

`FLAP_TRANSITIONS=0` turns the whole thing off: every transition goes out as
today.

### Holding transitions while flapping

While a camera has a `camera_flaps` row, every `camera_offline` and
`camera_recovered` it produces is **recorded with no delivery rows** and
`metadata.held = "flapping"`. The dashboard shows the row; the *Delivery*
column says it was held because the camera was flapping, which is a new
string, not the existing "no event_sink plugin registered". Each held
transition bumps the row's `changes` and `last_transition_at`.

The transition that crosses the threshold is held too: the flapping message
replaces it, and the earlier transitions in the window already went out as
themselves.

### Settling

Each pass, for every open `camera_flaps` row (`settled_at IS NULL`): if
`now - last_transition_at >= FLAP_SETTLE_SECONDS`, the camera has settled.
`camera_settled` is raised with the camera's current state (an open incident
means down), the count and the duration, and the row gets `settled_at = now`.
The next transition after that goes out as itself again, and only transitions
after `settled_at` count toward a new window.

A camera that is removed, or whose gateway is revoked, while flapping: the
row is dropped without an event. The removal is the news, and it has its own
path.

### State: `camera_flaps`

One migration, `0016_camera_flaps.sql`:

```sql
CREATE TABLE camera_flaps (
    camera_id          TEXT PRIMARY KEY,
    started_at         TEXT NOT NULL,    -- the transition that crossed the threshold
    changes            INTEGER NOT NULL, -- transitions since the window opened
    last_transition_at TEXT NOT NULL,
    settled_at         TEXT              -- NULL = flapping now
);
```

Explicit rather than derived from `fleet_events`: a flap is a fact the pass
acts on every minute, it has to survive a restart, and events are pruned on
their own schedule. One row per camera that has ever flapped: open while it
flaps, kept with `settled_at` afterwards so the next window knows where to
start counting, overwritten when it flaps again. A removed camera's row goes
with the camera.

### Where it does not apply

Gateways. A gateway that flaps takes every camera behind it with it, and
those cameras then flap by this rule; the gateway's own
`gateway_offline`/`gateway_recovered` still go out one per transition. The
same rule could be applied to gateways later with the same table and a
`kind` column; not now.

### Plugin SDK and sinks

`FleetEventKind` gains `CameraFlapping` and `CameraSettled` (`camera_flapping`,
`camera_settled` on the wire). The reference webhook sink needs no change: it
posts severity mark, title and detail. `docs/PLUGIN-SDK.md`'s list of kinds
grows by two.

`docs/PLUGIN-SDK.md` already promises that "a sink must ignore kinds it does
not know: more will be added without a protocol bump", and the Rust SDK's
`FleetEventKind` could not keep that promise: a sink built on it would fail
to parse the first `camera_flapping`, answer an error, be retried up the
ladder and given up on. So the enum also gains a `#[serde(other)]` fallback
variant (`Other`, `as_str` = `"other"`), with the conformance example and the
exhaustive matches updated, and the doc saying that a typed sink sees an
unknown kind as `Other` and should still answer `delivered: false` or post
the title. Found by the builder check; it is the project's own promise, so it
goes in here rather than being left unkept.

### Dashboard

- A held transition shows "held: the camera is flapping" in *Delivery*
  (`app.alerts.held`, en/es/ru).
- `camera_settled` with `state: "down"` is critical and gets the existing
  critical styling by severity; nothing else changes.

## Tests

- Three transitions inside the window: the third is stored with no delivery
  rows and `held`, and one `camera_flapping` is queued for every sink.
- While flapping, further transitions are stored, held, and counted; nothing
  else is queued.
- A pass after the settle window with the camera up raises `camera_settled`
  (info, state up, the count and duration in detail), clears the row, and the
  next transition goes out as itself.
- The same with the camera down raises a critical `camera_settled`.
- Transitions below the threshold go out exactly as before (the existing
  tests stand).
- `FLAP_TRANSITIONS=0`: nothing is ever held.
- Removing the camera while flapping drops the row and raises no settled
  event.
- After a settle, with a settle window shorter than the flap window, the next
  transition goes out as itself and does not re-open a flap by itself.
- SDK: an unknown kind on the wire parses as `Other`; the known ones round
  trip unchanged.
- Web: the held row reads as held, not as "no sink registered".

## Docs

`docs/ALERTS.md`: a section on a flapping camera (what goes out, the three
settings, that the dashboard still shows everything), and "Nothing
deduplicates a camera that flaps" leaves *What has never run* for a line
saying what does now and that no site has run it. The private working notes
get the same line in their verified/unverified list.

## Out of scope

Gateway flapping (above). Quiet hours, per-user rules, any other rule.
Collapsing in the dashboard. Changing what the reference sink posts.

## Left to the builder

- Whether `incident_pass` takes `now` as a parameter (as `delivery_pass`
  does) so the settle window can be walked in tests, or the tests backdate
  `last_transition_at` through the store. Passing `now` is the cleaner of the
  two and matches `delivery_pass`.
- How `raise_event` learns a transition is held: a second function, or a
  parameter. Either is fine; the held path must still write the event.
- The exact wording of the English titles above is settled; es and ru strings
  for the one new dashboard string are the builder's.
