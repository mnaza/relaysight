FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
# The release tag's version, when the release workflow passes one.
ARG GATEWAY_VERSION
RUN if [ -z "${GATEWAY_VERSION:-}" ]; then unset GATEWAY_VERSION; fi \
    && cargo build --release -p vms-gateway

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/vms-gateway /usr/local/bin/vms-gateway
CMD ["vms-gateway"]
