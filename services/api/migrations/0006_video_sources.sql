-- Video the gateway carries that is not a camera it discovered.
-- See docs/superpowers/specs/2026-09-22-video-sources-design.md.
--
-- No credential column, and none is coming: a source's password lives on the
-- gateway, encrypted, exactly as a camera's does.
CREATE TABLE video_sources (
    id         TEXT PRIMARY KEY,
    gateway_id TEXT NOT NULL REFERENCES gateways(id),
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,   -- rtsp | rtmp | srt
    address    TEXT NOT NULL,
    added_at   TEXT NOT NULL
);
CREATE INDEX idx_video_sources_gateway ON video_sources(gateway_id);
