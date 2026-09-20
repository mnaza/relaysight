-- Gateway revocation and the audit trail.
-- See docs/superpowers/specs/2026-09-10-revocation-audit-design.md.

ALTER TABLE gateways ADD COLUMN revoked_at TEXT;  -- NULL = not revoked

CREATE TABLE audit_log (
    id      TEXT PRIMARY KEY,   -- uuid
    at      TEXT NOT NULL,      -- fixed-width RFC 3339, as everywhere
    actor   TEXT NOT NULL,      -- 'admin' | 'gateway:<id>' | 'system'
    action  TEXT NOT NULL,      -- fixed strings; see the spec
    subject TEXT NOT NULL,      -- what it acted on ('' when none)
    detail  TEXT                -- human context; never a password or token
);
CREATE INDEX idx_audit_at ON audit_log(at);
