-- How each camera has been, by the hour. See docs/HEALTH.md.
--
-- Rollups rather than samples: 500 cameras reporting every twenty seconds is
-- two million rows a day, and nobody ever looks at one second in isolation.
-- What an operator asks is "how often has this been down this week", and an
-- hour is a fine grain for that.
--
-- Only time the gateway actually reported through is counted. A gateway that
-- was itself offline leaves a gap, and a gap is not uptime.
CREATE TABLE camera_health_hours (
    camera_id       TEXT NOT NULL,
    hour            TEXT NOT NULL,   -- RFC3339, the start of the hour, UTC
    healthy_seconds INTEGER NOT NULL DEFAULT 0,
    warning_seconds INTEGER NOT NULL DEFAULT 0,
    offline_seconds INTEGER NOT NULL DEFAULT 0,
    reconnects      INTEGER NOT NULL DEFAULT 0,
    fps_total       REAL    NOT NULL DEFAULT 0,
    bitrate_total   REAL    NOT NULL DEFAULT 0,
    samples         INTEGER NOT NULL DEFAULT 0,
    worst_loss      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (camera_id, hour)
);
CREATE INDEX idx_camera_health_hour ON camera_health_hours(hour);
