-- Events that left the building, and whether they got there.
-- See docs/superpowers/specs/2026-09-23-alerts-design.md.
--
-- An alert that vanishes because a sink was restarting is worse than no
-- alerts: silence then means "nothing happened" when it means "nobody was
-- listening". So an event is written down first and delivered afterwards.
CREATE TABLE fleet_events (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    severity    TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    site_id     TEXT NOT NULL,
    site_name   TEXT NOT NULL,
    gateway_id  TEXT,
    camera_id   TEXT,
    title       TEXT NOT NULL,
    detail      TEXT,
    metadata    TEXT NOT NULL
);
CREATE INDEX idx_fleet_events_occurred ON fleet_events(occurred_at);

-- One row per event per sink. A sink registered after an event was raised
-- gets no row: alerts are about now, not about history.
CREATE TABLE event_deliveries (
    event_id        TEXT NOT NULL REFERENCES fleet_events(id) ON DELETE CASCADE,
    plugin_id       TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    delivered_at    TEXT,
    declined        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at TEXT,
    PRIMARY KEY (event_id, plugin_id)
);
CREATE INDEX idx_event_deliveries_due ON event_deliveries(next_attempt_at);
