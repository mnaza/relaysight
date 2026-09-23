-- How each camera is recorded: continuously into the gateway's ring buffer, or
-- only when something asks. See
-- docs/superpowers/specs/2026-09-23-recording-policies-design.md.
--
-- `keep` is JSON because the rules are a list of shapes that will grow, and
-- nothing in the control plane ever reads inside them: the gateway does.
CREATE TABLE recording_policies (
    camera_id      TEXT PRIMARY KEY,
    gateway_id     TEXT NOT NULL,
    mode           TEXT NOT NULL,   -- off | continuous
    keep           TEXT NOT NULL,   -- JSON array of keep rules
    retention_days INTEGER NOT NULL,
    updated_at     TEXT NOT NULL
);
CREATE INDEX idx_recording_policies_gateway ON recording_policies(gateway_id);
