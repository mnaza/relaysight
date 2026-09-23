-- Where a scheduled clip goes. The gateway has no opinion about which storage
-- plugin to use and no way to learn one, so the policy carries it.
ALTER TABLE recording_policies ADD COLUMN storage_plugin_id TEXT NOT NULL DEFAULT '';
