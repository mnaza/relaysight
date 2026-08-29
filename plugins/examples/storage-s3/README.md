# S3-compatible storage plugin

Provides presigned transfers instead of proxying video/snapshot bytes through the VMS API. The edge gateway, browser, or another service uploads/downloads directly to the configured object store.

Works with S3-compatible APIs such as AWS S3, MinIO and Backblaze B2 S3 by changing environment variables.

## Transfer audiences

Plugin Protocol v1 supports `audience` on upload/download requests:

- `browser` — URL must be reachable from the user's browser.
- `edge` — URL must be reachable from the remote gateway/site.
- `service` — URL is consumed by control-plane/internal services; this is the backward-compatible default.

This matters in local Docker and real deployments where one object store may have different internal, public, and edge-facing hostnames.

Environment:

- `S3_BUCKET`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` — required.
- `S3_ENDPOINT` — base/internal endpoint.
- `S3_PUBLIC_ENDPOINT` — endpoint used when signing for `browser`.
- `S3_EDGE_ENDPOINT` — endpoint used when signing for `edge`.
- `S3_SERVICE_ENDPOINT` — endpoint used when signing for `service`.
- `AWS_REGION`, `S3_PREFIX`, `S3_ADDRESSING_STYLE`, `PLUGIN_TOKEN` — optional.
