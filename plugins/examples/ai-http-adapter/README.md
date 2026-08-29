# AI HTTP adapter plugin

This is the shortest path for connecting your own AI service to either Community or Commercial editions.

The VMS sends the versioned `AiAnalyzeRequest` contract to this plugin. The current edge flow fetches an ONVIF snapshot and sends it to the plugin as `MediaInput::InlineBase64`, so the AI container does not need direct access to the camera or object storage.

## Modes

- **Real provider:** set `UPSTREAM_AI_URL`; requests are forwarded to your inference endpoint. `UPSTREAM_AI_TOKEN` is optional bearer auth.
- **Simulated local demo:** set `SIMULATED_AI=true` with no upstream URL. The plugin returns clearly marked synthetic bounding boxes so the full UI pipeline can be demonstrated without a model.
- **Safe no-op:** with no upstream and simulation disabled, the plugin returns zero detections.

Environment:

- `PLUGIN_TOKEN` — optional bearer auth between VMS core and plugin.
- `UPSTREAM_AI_URL` — your inference endpoint.
- `UPSTREAM_AI_TOKEN` — optional upstream bearer token.
- `SIMULATED_AI` — `true` only for explicit local demos; keep `false` in production.

If your upstream already returns a `detections` array in the plugin protocol shape it passes through. Otherwise the raw response is exposed in `metadata.upstream` so an adapter can normalize it.
