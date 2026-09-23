# Alerts: an incident reaches someone who is not looking

**Goal:** a camera goes offline, an incident opens, the dashboard shows it —
and nobody knows unless they happen to be looking at the dashboard. A fleet
that only reports to a screen nobody is watching reports to nobody. This makes
events leave the building.

**Decided with the user (2026-09-23):** delivery lives in a **plugin**, not in
the API. `event_sink` is already a capability in the protocol and has never
been implemented; this implements it. No SMTP password, chat token or webhook
URL enters the control plane's database, which is the same line the project
draws around camera passwords, and a Community build does not grow a mail
client.

## What exists today

- `PluginCapability::EventSink` exists in `crates/plugin-sdk`, with no request
  or response type behind it. Nothing calls it.
- The plugin runtime reaches plugins over HTTP at `/v1/...` with a bearer
  token from the environment, and knows which capabilities each one declares.
- Incidents open and close in the API when telemetry says a camera changed
  state; `IncidentView` is what the dashboard reads.
- The gateways screen computes `online` from `last_seen` when it is read, so
  nothing anywhere notices the moment a gateway stops reporting.
- Reference plugins live in their own repository (`relaysight-plugins`), which
  is where a webhook plugin belongs.

## Design

### The event

```rust
pub struct FleetEvent {
    pub id: String,                 // uuid, and the idempotency key
    pub kind: FleetEventKind,       // camera_offline | camera_recovered |
                                    // gateway_offline | gateway_recovered
    pub severity: EventSeverity,    // info | warning | critical
    pub occurred_at: DateTime<Utc>,
    pub customer_id: String,
    pub site_id: String,
    pub site_name: String,
    pub gateway_id: Option<String>,
    pub camera_id: Option<String>,
    /// One line, already written for a human: "Yard camera stopped answering
    /// at Bakery, Barcelona".
    pub title: String,
    pub detail: Option<String>,
    pub metadata: serde_json::Value,
}
```

A plugin gets `POST /v1/events` with `{context, event}` and answers
`{delivered, detail}`. The `id` is stable across retries, so a sink that has
already posted a message can say so instead of posting it twice.

### Raising events

- **Cameras**: where incidents already open and close. An incident is the same
  fact; an event is that fact leaving the building.
- **Gateways**: a watcher in the API, because nothing notices a gateway going
  quiet — the screen works it out on read. It runs on a timer, compares
  `last_seen` against the same threshold the dashboard uses, and raises the
  transition once.

### Delivering them

An **outbox**, not a fire-and-forget call. An alert that vanishes because a
plugin was restarting is worse than no alerts: the operator believes silence
means nothing happened.

```sql
CREATE TABLE fleet_events (...);            -- the event itself
CREATE TABLE event_deliveries (             -- one row per event per plugin
    event_id, plugin_id, attempts, delivered_at, last_error, next_attempt_at
);
```

A dispatcher task walks rows that are due, calls each `event_sink` plugin, and
backs off — seconds, then minutes — giving up after a handful of attempts and
leaving the failure on the row where the dashboard can show it. A plugin
registered after an event was raised does not get history: alerts are about
now.

### The dashboard

An *Alerts* panel: the last events, their severity, and whether each sink took
them, with the error where it did not. A **send a test event** button, because
the question anyone asks first is whether the thing is wired up at all.

### The plugin itself

A webhook sink in `relaysight-plugins`: posts a small JSON body to a URL from
its own environment, which is all Slack, Discord, Telegram bridges and
internal endpoints need. It belongs there, with the other reference plugins,
and this repository only defines the contract it implements.

## Testing

TDD as always.

- Contract: an event serialises to the documented shape and back.
- Outbox: an event is stored once and queued for every enabled sink; a sink
  that fails is retried with a growing delay, is not retried before it is due,
  and is given up on after the last attempt with the reason kept.
- Idempotency: the same event delivered twice carries the same id.
- Raising: an incident opening raises exactly one event; a camera flapping
  raises one per transition, not one per telemetry batch.
- Gateway watcher: a gateway that stops reporting raises one event, not one
  per tick, and raises a recovery when it comes back.
- A registration added after the event was raised gets nothing.
- Web: the panel lists events and delivery state; the test button posts.

## Out of scope

Alert rules and quiet hours — a sink that wants filtering can filter; the
control plane sending the same event to everything is honest and simple.
Per-user subscriptions, which need user accounts beyond the single admin.
SMS. Deduplicating a camera that flaps every minute: that is a real problem,
and it belongs in a rule engine that does not exist yet.
