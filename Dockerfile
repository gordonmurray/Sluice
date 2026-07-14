# Builds the sluice gateway and test client. The host only needs docker and
# cargo; the release binaries are produced here.

FROM rust:1.96-bookworm AS builder

WORKDIR /build
COPY . .
RUN cargo build --release --workspace

# --- runtime ---
FROM debian:bookworm-slim

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
