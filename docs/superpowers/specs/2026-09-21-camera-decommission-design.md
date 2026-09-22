# Retiring a revoked gateway's cameras

**Goal:** an installer who revokes a gateway has no way to clean up after it.
The token stops working and the cameras stop being tracked, but they sit in the
fleet forever, reported by nobody. This gives them a way out of the roster that
does not lose what they were.

Backlog: "Camera decommission flow — retire a revoked gateway's cameras from
the roster".

**Decisions taken with the user (2026-09-21):**

- **A separate action, not part of revoke.** Revoking stays a security step:
  stop the token, stop the tracking. Retiring the cameras is an explicit second
  action on an already-revoked gateway, so revoking to swap hardware does not
  wipe the roster that the replacement is about to repopulate.
- **A tombstone, not a delete.** `cameras.retired_at`, mirroring
  `gateways.revoked_at`. A recording keeps pointing at a camera that can still
  be described.
- **Telemetry brings a camera back.** A replacement gateway reporting the same
  camera clears the stamp, moves the camera to the new gateway, and keeps its
  `first_seen` and its recordings.

## What exists today

- `revoke_gateway` (`services/api/src/main.rs`) tombstones the gateway, closes
  its cameras' incidents, and drops its live presence from memory. Its cameras
  stay in `cameras` and keep appearing in `/api/v1/fleet` and
  `/api/v1/cameras`, with no one reporting them.
- `upsert_fleet_identity` (`store/sqlite.rs`) upserts org, site, gateway and
  cameras from every telemetry batch. A camera's `first_seen` is never updated
  after insert; `gateway_id` is.
- `recordings.camera_id` has no foreign key to `cameras` (`0001_fleet.sql`), so
  retiring or even deleting a camera leaves playback working.
- Revocation's web side: `web/dashboard-app.js` renders gateways with a Revoke
  button and an alert on failure (`app.gateways.revokeFailed`), in en/es/ru.

## Design

### The tombstone

Migration `0005_camera_retirement.sql`:

```sql
ALTER TABLE cameras ADD COLUMN retired_at TEXT;  -- NULL = in service
```

`fleet_identity` and `fleet_cameras` gain `WHERE retired_at IS NULL`. Every
reader of the roster goes through those two, so a retired camera leaves the
dashboard, the camera list, the incident pass and the entitlement count at
once.

`upsert_fleet_identity`'s camera upsert adds `retired_at = NULL` to its
`ON CONFLICT DO UPDATE SET`: the camera reporting again is what un-retires it,
and no other path does.

### The action

`POST /api/v1/gateways/{gateway_id}/cameras/retire`, session-protected like
the rest of the roster.

- An id the roster does not have answers **404**, as a revoke does. A gateway
  that exists but is still in service answers **409**, with nothing changed —
  retiring the cameras of a working gateway is not a thing anyone means to do.
- Otherwise it closes any incident still open for that gateway's cameras, then
  stamps every one whose `retired_at` is NULL, and answers **200** with
  `{"retired": <count>}`. Run twice, the second answers `{"retired": 0}`.
- Audit row: `cameras.retired`, subject the gateway id, detail the count.

New `Store` method:

```rust
async fn retire_gateway_cameras(
    &self,
    gateway_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<String>, StoreError>;
```

It returns the ids it stamped, so the count is the length. The incidents are
closed before the stamping, not after: a retired camera has left the roster the
incident pass walks, so an incident that failed to close after the stamp would
stay open forever, while one that fails before it leaves nothing changed.

### The dashboard

On the gateways view, a revoked gateway gains a second button, *Retire
cameras*, beside the revoked state it already shows. It confirms first — the
copy says the cameras leave the roster and come back if a gateway reports them
again — then calls the endpoint, refreshes, and on failure raises an alert the
way a failed revoke does. Strings in en/es/ru:
`app.gateways.retireCameras`, `app.gateways.retireConfirm`,
`app.gateways.retireFailed`.

## Testing

TDD as always.

Store (`services/api/src/store/`): retiring stamps only that gateway's
cameras and returns their ids; a second call returns none; retired cameras
leave `fleet_cameras` and `fleet_identity`; telemetry naming a retired camera
clears the stamp, moves it to the reporting gateway, and keeps `first_seen`.

API (`services/api/src/main.rs` tests): retiring a gateway that is not revoked
is 409 and changes nothing; retiring a revoked one answers the count, empties
it from `/api/v1/cameras` and `/api/v1/fleet`, closes its open incidents, and
leaves a `cameras.retired` audit row; its recordings still resolve through
`/api/v1/recordings/{id}/playback`.

Web (`web/tests/gateways.test.mjs`): the button shows only for a revoked
gateway; clicking it calls the endpoint and refreshes; a failure raises the
alert. Plus the existing i18n parity test, which covers the new strings.

## Out of scope

Deleting cameras outright, and any retention or cleanup of their recordings —
`DEFAULT_RETENTION_DAYS` already expires those on its own schedule. Retiring
individual cameras rather than a gateway's whole set. Retiring a site or an
organization that has been left empty: an empty site still shows, and that is
today's behaviour for a site whose cameras have simply gone quiet.
