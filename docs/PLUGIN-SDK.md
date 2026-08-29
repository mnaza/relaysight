# Plugin SDK v1

Plugins are a **Community Core feature** and therefore work in both Community Self-Hosted and Commercial editions.

## Why out-of-process plugins

The core does not load arbitrary native `.so`/`.dll` libraries. Rust has no stable native ABI for this use case, and loading third-party code into the VMS process would make crashes and security isolation much harder. A plugin is an HTTP service (normally a container or local process) with a versioned manifest and capability endpoints.

This makes language/runtime choice independent of the VMS: AI can be Python + CUDA/TensorRT, storage can be Rust/Go/Python, and a partner can deploy a plugin next to a self-hosted installation without rebuilding core.

## Discovery

The API reads `PLUGIN_DIR` (default `plugins.d`). Each `*.json` file is a `PluginRegistration`:

```json
{
  "endpoint": "http://my-ai:9001",
  "placement": "either",
  "enabled": true,
  "token_env": "MY_AI_PLUGIN_TOKEN",
  "manifest": {
    "id": "my-ai",
    "name": "My detector",
    "version": "1.0.0",
    "protocol_version": 1,
    "vendor": "My company",
    "description": "People + vehicle detector",
    "capabilities": ["ai_analyze"]
  }
}
```

At reload/start the registry attempts `GET /v1/plugin/manifest`. The embedded manifest is a fallback so an offline plugin is still visible in operations UI.

## Common endpoints

- `GET /v1/plugin/manifest`
- `GET /v1/plugin/health`

Optional bearer authentication is configured with `token_env`; the secret stays in the API environment and is never returned to the browser.

## AI capability

Capability: `ai_analyze`

Endpoint: `POST /v1/ai/analyze`

The input is a media reference, not an assumption about a specific model. A frame can be exposed as a short-lived URL or as an object reference in a storage plugin. The response is normalized to model name + detections + arbitrary metadata.

The included `plugins/examples/ai-http-adapter` can forward the contract to your existing AI endpoint.

## Storage capability

Capability: `storage_blob`

Endpoints:

- `POST /v1/storage/uploads`
- `POST /v1/storage/downloads`
- `POST /v1/storage/delete`

Storage plugins should preferably return **presigned transfers**. Video bytes then travel `gateway/browser ↔ storage` directly instead of `gateway → API → storage`. This keeps the control plane cheap and lets a self-hosted customer choose S3, MinIO, B2 or a custom storage service.

The included `plugins/examples/storage-s3` implements this contract for S3-compatible storage.

## Versioning

Current protocol version: `1` (`vms-plugin-sdk::PLUGIN_PROTOCOL_VERSION`).

Breaking wire changes require a new protocol version. Adding optional JSON fields is expected to remain backward-compatible.

## Future capabilities

The enum already reserves an `event_sink` capability. Natural next extensions are:

- event/webhook sinks
- identity/SSO connectors
- custom camera drivers
- license-plate OCR
- alert routing
- archive lifecycle/tiering

## Tenant-aware bindings

Invocation requests contain an optional `PluginInvocationContext` with `organization_id`, `site_id`, `camera_id`, `connection_id` and `trace_id`.

This is important for the Commercial multi-tenant path: one plugin implementation may serve many organizations while `connection_id` selects that organization's vault-backed model/storage connection. In Community a deployment can simply use one global plugin and leave the context mostly empty.

Once production user auth exists, the core must derive organization context from authenticated state rather than trusting tenant IDs supplied by a browser request.

## Placement: control plane vs edge

A registration has `placement: control_plane | edge | either`.

- **control_plane** — storage connectors, webhooks, SaaS integrations.
- **edge** — inference or proprietary drivers that must stay inside the customer LAN.
- **either** — e.g. an AI detector that can run centrally or beside a gateway/GPU.

The current prototype dispatches from the control API. The runtime crate is intentionally independent so the same dispatcher can be embedded into the gateway when the frame scheduler is implemented; the wire protocol does not need to change.

## Transfer audience (v6 additive field)

Storage upload/download requests may include:

```json
{"audience":"browser"}
```

Allowed values are `browser`, `edge`, and `service`; omitted means `service`. This lets one plugin sign the same S3-compatible bucket with different reachable endpoints. Example: browser uses `https://storage.example.com`, edge uses a site/VPN endpoint, and internal services use `http://minio:9000`.

Environment variables in the reference S3 plugin:

- `S3_ENDPOINT` — control/internal client used for bucket operations.
- `S3_PUBLIC_ENDPOINT` — signer endpoint for `browser`.
- `S3_EDGE_ENDPOINT` — signer endpoint for `edge`.
- `S3_SERVICE_ENDPOINT` — signer endpoint for `service`.
