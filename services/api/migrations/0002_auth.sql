-- Auth: the single admin credential and its sessions.
-- See docs/superpowers/specs/2026-09-07-dashboard-auth-design.md.

CREATE TABLE admin_credential (
    id            INTEGER PRIMARY KEY CHECK (id = 1),  -- single row by construction
    password_hash TEXT NOT NULL,                       -- argon2id PHC string
    updated_at    TEXT NOT NULL
);

CREATE TABLE sessions (
    id_hash    TEXT PRIMARY KEY,  -- SHA-256 hex of the session id; the id itself never lands here
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_expiry ON sessions(expires_at);
