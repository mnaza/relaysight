-- People, rather than one shared password.
-- See docs/superpowers/specs/2026-09-24-users-and-roles-design.md.
--
-- The single admin credential stays where it is: the seed carries it into a
-- user so that nobody's password changes on the way through.
CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,   -- lowercased on the way in
    password_hash TEXT NOT NULL,          -- argon2id PHC, as before
    role          TEXT NOT NULL,          -- owner | technician | viewer
    customer_id   TEXT,                   -- set: this user sees only that customer
    created_at    TEXT NOT NULL,
    disabled_at   TEXT
);

-- A session that does not know whose it is cannot say who did anything.
ALTER TABLE sessions ADD COLUMN user_id TEXT;
