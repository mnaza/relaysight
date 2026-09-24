-- A hash chain over the audit log.
--
-- The log was a table, and a table is something anybody with database access
-- can edit without leaving a mark. Each row now carries the hash of the row
-- before it, so changing, removing or reordering one breaks every hash after
-- it and verification says exactly where.
--
-- Rows written before this migration have no hashes. They are reported as
-- unchained rather than as broken: claiming to have protected something that
-- was written before the protection existed would be the dishonest reading.
ALTER TABLE audit_log ADD COLUMN prev_hash TEXT;
ALTER TABLE audit_log ADD COLUMN hash TEXT;
