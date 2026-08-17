# Nostos — multi-stage Dockerfile for the sync server + cloud control plane +
# push daemon (ADR-0038).
#
# Builds all three binaries from the workspace in a single builder stage, then
# copies them into a slim runtime image. The `pg` feature is enabled on
# nostos-server so the real PgReplicator ships in the image.
#
#   docker build -t nostos .
#   docker run --rm nostos nostos-server   # default entrypoint arg
#   docker run --rm nostos nostos-cloud
#   docker run --rm nostos nostos-pushd

# ---------- builder ----------
FROM rust:1.95-bookworm AS builder
WORKDIR /nostos
# Install needed system libs (none beyond what the base image provides for our
# deps; rusqlite uses `bundled` sqlite, reqwest uses rustls — no system deps).
COPY . .
# Build both binaries with the pg feature on nostos-server. Release profile.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/nostos/target \
    cargo build --release -p nostos-server --features pg -p nostos-cloud -p nostos-push && \
    cp target/release/nostos-server /usr/local/bin/ && \
    cp target/release/nostos-cloud  /usr/local/bin/ && \
    cp target/release/nostos-pushd  /usr/local/bin/

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/nostos-server /usr/local/bin/nostos-server
COPY --from=builder /usr/local/bin/nostos-cloud  /usr/local/bin/nostos-cloud
COPY --from=builder /usr/local/bin/nostos-pushd  /usr/local/bin/nostos-pushd
# Default to the sync server; override CMD for the cloud/push binaries.
ENV NOSTOS_LOG=info,nostos=info RUST_LOG=info
EXPOSE 8800 9090 8090
ENTRYPOINT ["nostos-server"]
