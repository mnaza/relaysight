-- A camera whose source was deliberately removed, as opposed to one retired
-- because its gateway went away. Telemetry clears `retired_at` for every
-- camera it mentions, so that column cannot hold a removal: the gateway keeps
-- reporting from its cached source list for a round or two after the delete,
-- and the row came straight back. `removed_at` survives telemetry and is
-- cleared only by adding the source again.
ALTER TABLE cameras ADD COLUMN removed_at TEXT;
