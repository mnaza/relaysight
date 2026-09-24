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

## Registering a plugin

Two places, and the second is new:

- **`plugins.d/*.json` on the box.** The bootstrap. A control plane whose
  database is having a bad day still comes up with the plugins it was
  installed with.
- **Connected from the dashboard**, by an owner. The control plane asks the
  endpoint what it is before writing anything down, so a registration for
  something that never answered is not a row nobody can explain a month later.
  The id comes from the plugin's own manifest, not from the form.

A stored registration wins over a file with the same id: somebody typed it
more recently. Removing it brings the file's version back on the next reload.

A registration can name a customer. That customer's cameras then use that
plugin — their bucket, their model — for recording, clipping and analysis,
and everybody else keeps the default. A scoped plugin that does not provide
the capability being asked for is skipped rather than handed storage work
because it happened to be theirs.

An explicit `storage_plugin_id` in a request still wins over both: somebody
naming a plugin means it.

## Checking your plugin

```bash
make check-plugin ENDPOINT=http://localhost:9002
PLUGIN_TOKEN_ENV=STORAGE_PLUGIN_TOKEN make check-plugin ENDPOINT=…
```

It asks the endpoint what it is, checks the protocol version, calls health,
and then exercises every capability the manifest declares — a one-pixel image
for `ai_analyze`, a signing round trip for `storage_blob`, a `test` event for
`event_sink`. It signs and asks; it never uploads bytes, and it does not
exercise storage delete, which would delete something.

A sink answering `delivered: false` passes: declining is a correct answer.

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

## Event sink capability

Capability: `event_sink`

Endpoint: `POST /v1/events`

The control plane hands over facts; the plugin decides who hears about them
and how. That is why no SMTP password, chat token or webhook URL is ever
stored here: it belongs to the sink, in the sink's own environment.

```json
{
  "context": {"site_id": "site-1", "trace_id": "evt-9f3c"},
  "event": {
    "id": "9f3c…",
    "kind": "camera_offline",
    "severity": "critical",
    "occurred_at": "2026-09-23T09:41:02Z",
    "customer_id": "cust-1",
    "site_id": "site-1",
    "site_name": "Bakery",
    "gateway_id": "gw-1",
    "camera_id": "cam-1",
    "title": "Yard camera stopped answering at Bakery",
    "detail": "RTSP probe failed",
    "metadata": {"reconnects": 3}
  }
}
```

The answer is `{"delivered": true}`, or `{"delivered": false, "detail": "…"}`
for an event the sink decided is not for it — which is not an error and is not
retried. Anything other than a 2xx **is** retried, with a growing delay.

`title` is written for a human already: a sink that posts nothing but that
line is useful. `id` is stable across every retry and every sink, so a sink
that has already posted a message can recognise it rather than posting twice.

Event kinds today: `camera_offline`, `camera_recovered`, `gateway_offline`,
`gateway_recovered`, and `test` for the dashboard's "send a test event". A
sink must ignore kinds it does not know: more will be added without a protocol
bump.

## Running a plugin beside the control plane

A plugin is somebody else's code in the same deployment. The compose plugin
profile gives each one a memory limit, a CPU share and a process cap, so a
model that leaks or a sink that spins costs its own container rather than the
box:

```bash
PLUGIN_MEM_LIMIT=2g PLUGIN_CPU_LIMIT=4 docker compose --profile plugins up -d
```

Inference gets 1 GiB and two CPUs by default; signing URLs and posting
webhooks get 256 MiB and half a CPU, because that is what they do. Nothing
here stops a plugin reaching the network — that is a network policy, and it
belongs to whatever runs the containers.

## Service identity

A plugin call is an HTTP request to somebody else's service carrying a bearer
token. On a shared network that is one leaked token away from being replayed
by anything that can reach the endpoint, so a deployment can present a client
certificate and trust its own CA:

| | |
| --- | --- |
| `PLUGIN_CLIENT_IDENTITY` | a PEM holding the client certificate chain and its private key |
| `PLUGIN_CA_BUNDLE` | a PEM of roots to trust, beside the system ones |

Either being set and unreadable, not a PEM, or — for the bundle — holding no
certificates at all stops the control plane starting. A control plane that
cannot read its client certificate and carries on without one has mTLS in the
documentation and not on the wire. An empty CA file is the quiet version of
the same thing: it parses, trusts nothing new, and says nothing.

`make check-plugin-mtls` proves the certificate reaches the far end: it runs a
plugin that demands client auth and checks both that a call with a certificate
gets in and that one without is refused.

## Timeouts and what happens when a plugin is down

Calls are given the time their kind deserves rather than one flat number:
three seconds for a manifest or a health check, eight for signing a URL or
delivering an event, and thirty — `PLUGIN_AI_TIMEOUT_SECONDS` — for running a
model. One number for all of them means either a model gets cut off or a dead
plugin holds a request open for half a minute.

After three failures in a row the control plane stops calling that plugin for
a cooling period: fifteen seconds, doubling to at most five minutes. While it
is cooling off, calls fail at once with a message saying how long is left, and
the dashboard's plugin card says the same. A success clears it.

A plugin that is down otherwise costs every caller the full timeout, one after
another, for as long as it stays down. Reloading `plugins.d` does not clear
the state: editing a manifest is not evidence that the plugin came back.

## Versioning

Current protocol version: `1` (`vms-plugin-sdk::PLUGIN_PROTOCOL_VERSION`).

Breaking wire changes require a new protocol version. Adding optional JSON fields is expected to remain backward-compatible.

## Future capabilities

Natural next extensions are:

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
