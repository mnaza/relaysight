# Commercial overlay

This directory is intentionally outside the open-source Cargo workspace.

The current `control-plane` is a minimal prototype of the private service boundary. It provides tenant/customer entitlements to Community Core over `ENTITLEMENTS_URL`. The open core does not contain billing rules or proprietary feature logic.

Long-term private responsibilities belong here (or in a separate private repository):

- subscription/billing integration
- signed commercial licenses
- managed-cloud tenancy
- reseller hierarchy
- advanced white-label/domain management
- SSO/OIDC/SAML
- audit/compliance policy
- HA/SLA operations
- support entitlements

Do **not** move the plugin SDK/runtime here. Custom AI and storage plugins are intentionally supported by both editions.
