FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p vms-api

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/vms-api /usr/local/bin/vms-api
ENV API_BIND=0.0.0.0:8080
EXPOSE 8080
CMD ["vms-api"]
