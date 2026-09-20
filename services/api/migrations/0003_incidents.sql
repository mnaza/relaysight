-- Camera disconnect/recovery history.
-- See docs/superpowers/specs/2026-09-09-incident-timeline-design.md.

CREATE TABLE incidents (
    id         TEXT PRIMARY KEY,   -- uuid
    camera_id  TEXT NOT NULL,
    started_at TEXT NOT NULL,      -- fixed-width RFC 3339, as everywhere
    ended_at   TEXT,               -- NULL = ongoing
    detail     TEXT
);
-- One open incident per camera, as a database invariant.
CREATE UNIQUE INDEX idx_incidents_open ON incidents(camera_id) WHERE ended_at IS NULL;
CREATE INDEX idx_incidents_started ON incidents(started_at);
