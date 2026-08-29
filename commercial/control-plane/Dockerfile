FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml ./Cargo.toml
COPY crates ./crates
COPY commercial/control-plane ./commercial/control-plane
WORKDIR /src/commercial/control-plane
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/commercial/control-plane/target/release/vms-commercial-control-plane /usr/local/bin/control-plane
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/control-plane"]
