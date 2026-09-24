-- Plugins an owner connected, as opposed to ones the box was installed with.
-- See docs/PLUGIN-SDK.md.
--
-- plugins.d stays: it is the bootstrap, and a control plane whose database is
-- having a bad day should still come up with the plugins it was installed
-- with. A row here wins over a file with the same id.
CREATE TABLE plugin_registrations (
    plugin_id   TEXT PRIMARY KEY,
    endpoint    TEXT NOT NULL,
    placement   TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    token_env   TEXT,
    customer_id TEXT,              -- set: offered to that customer only
    created_at  TEXT NOT NULL
);
