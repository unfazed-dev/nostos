# Nostos — multi-stage Dockerfile for the sync server + cloud control plane.
#
# Builds both binaries from the workspace in a single builder stage, then copies
# them into a slim runtime image. The `pg` feature is enabled on nostos-server so
# the real PgReplicator ships in the image.
#
#   docker build -t nostos .
#   docker run --rm nostos nostos-server   # default entrypoint arg
#   docker run --rm nostos nostos-cloud

# ---------- builder ----------
FROM rust:1.95-bookworm AS builder
WORKDIR /nostos
# Install needed system libs (none beyond what the base image provides for our
# deps; rusqlite uses `bundled` sqlite, reqwest uses rustls — no system deps).
COPY . .
# Build both binaries with the pg feature on nostos-server. Release profile.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/nostos/target \
    cargo build --release -p nostos-server --features pg -p nostos-cloud && \
    cp target/release/nostos-server /usr/local/bin/ && \
    cp target/release/nostos-cloud  /usr/local/bin/

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/nostos-server /usr/local/bin/nostos-server
COPY --from=builder /usr/local/bin/nostos-cloud  /usr/local/bin/nostos-cloud
# Default to the sync server; override CMD for the cloud binary.
ENV NOSTOS_LOG=info,nostos=info RUST_LOG=info
EXPOSE 8800 9090
ENTRYPOINT ["nostos-server"]
