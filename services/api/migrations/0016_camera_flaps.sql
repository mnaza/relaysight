-- A camera that keeps going up and down, and whether it has stopped.
-- See docs/superpowers/specs/2026-11-10-flapping-camera-design.md.
--
-- Explicit rather than derived from fleet_events: a flap is a fact the
-- incident pass acts on every minute, it has to survive a restart, and events
-- are pruned on their own schedule. One row per camera that has ever flapped:
-- open while it flaps, kept with settled_at afterwards so the next window
-- knows where to start counting, overwritten when it flaps again.
CREATE TABLE camera_flaps (
    camera_id          TEXT PRIMARY KEY,
    started_at         TEXT NOT NULL,    -- the transition that crossed the threshold
    changes            INTEGER NOT NULL, -- transitions since the window opened
    last_transition_at TEXT NOT NULL,
    settled_at         TEXT              -- NULL = flapping now
);
