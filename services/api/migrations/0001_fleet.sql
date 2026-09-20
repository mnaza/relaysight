CREATE TABLE organizations (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE sites (
    id     TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES organizations(id),
    name   TEXT NOT NULL,
    city   TEXT NOT NULL
);
CREATE INDEX idx_sites_org ON sites(org_id);

CREATE TABLE gateways (
    id          TEXT PRIMARY KEY,
    site_id     TEXT NOT NULL REFERENCES sites(id),
    hostname    TEXT,
    version     TEXT,
    -- Empty string means "seen in telemetry, never enrolled": such a gateway
    -- authorizes only via the bootstrap GATEWAY_TOKEN. A real hash is 64 hex
    -- chars, so no presented token can ever match ''.
    token_hash  TEXT NOT NULL DEFAULT '',
    enrolled_at TEXT,
    last_seen   TEXT
);

CREATE TABLE enrollments (
    token_hash TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL,
    org_name   TEXT NOT NULL,
    site_id    TEXT NOT NULL,
    site_name  TEXT NOT NULL,
    city       TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    claimed    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE cameras (
    id           TEXT PRIMARY KEY,
    gateway_id   TEXT NOT NULL REFERENCES gateways(id),
    site_id      TEXT NOT NULL,
    name         TEXT NOT NULL,
    manufacturer TEXT,
    model        TEXT,
    firmware     TEXT,
    codec        TEXT,
    width        INTEGER,
    height       INTEGER,
    first_seen   TEXT NOT NULL,
    last_seen    TEXT NOT NULL
);
CREATE INDEX idx_cameras_gateway ON cameras(gateway_id);

CREATE TABLE recordings (
    id           TEXT PRIMARY KEY,
    camera_id    TEXT NOT NULL,
    started_at   TEXT NOT NULL,
    ended_at     TEXT NOT NULL,
    delete_after TEXT,
    codec        TEXT NOT NULL,
    manifest     TEXT NOT NULL
);
CREATE INDEX idx_recordings_camera ON recordings(camera_id, started_at);
CREATE INDEX idx_recordings_expiry ON recordings(delete_after);
