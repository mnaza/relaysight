-- Retiring the cameras of a revoked gateway.
-- See docs/superpowers/specs/2026-09-21-camera-decommission-design.md.
--
-- A tombstone rather than a delete: recordings point at camera ids, and a
-- recording whose camera cannot be described is worse than a row nobody shows.
ALTER TABLE cameras ADD COLUMN retired_at TEXT;  -- NULL = in service
