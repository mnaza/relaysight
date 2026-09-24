-- Where a plugin's token is, when it is not in the control plane's
-- environment. Vault's agent, Kubernetes secrets and Docker secrets all
-- present a secret as a file.
ALTER TABLE plugin_registrations ADD COLUMN token_file TEXT;
