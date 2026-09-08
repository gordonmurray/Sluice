# Build all application binaries inside Docker. No host Rust toolchain is required.

FROM rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY gateway/ gateway/
COPY rules/ rules/
COPY client/ client/
COPY indexer/ indexer/
COPY demo-origin/ demo-origin/
COPY migrations/ migrations/
RUN cargo build --release --workspace --locked

# --- runtime ---
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/gateway /usr/local/bin/gateway
COPY --from=builder /build/target/release/client /usr/local/bin/client
COPY --from=builder /build/target/release/indexer /usr/local/bin/indexer
COPY --from=builder /build/target/release/demo-origin /usr/local/bin/demo-origin

ENV BIND=0.0.0.0:8080
ENV RUST_LOG=info

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/gateway"]
