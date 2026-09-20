# syntax=docker/dockerfile:1
# One build for every Rust binary, selected with `--target`:
#   docker build -f deploy/Dockerfile --target fiber-api   .
#   docker build -f deploy/Dockerfile --target fiber-agent .
# cargo-chef gives the dependency graph its own layer, so editing crates/ does not
# rebuild aws-sdk-s3 / sqlx / opentelemetry every time.
FROM rust:1.98-bookworm AS chef
WORKDIR /app
RUN cargo install cargo-chef --locked --version "^0.1"

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p fiber-api -p fiber-agent -p fiber-cli \
    && strip target/release/fiber-api target/release/fiber-agent target/release/fiber

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Runs as uid 10001, the same non-root uid as the agent stage. The only path the API
# writes at runtime is FIBER_ARTIFACTS_DIR: the local artifact backend puts blobs there,
# and main.rs create_dir_all()s it at boot even under the S3 backend (a no-op on a
# directory that already exists). Everything else it touches is Postgres, Redis and the
# network, so the rootfs can be mounted read-only — see deploy/docker-compose.yml.
# Upgrading an existing deployment whose artifact volume was created by a root image
# needs a one-shot chown; docs/operations.md#upgrades has the command.
FROM runtime AS fiber-api
COPY --from=builder /app/target/release/fiber-api /usr/local/bin/fiber-api
RUN useradd --create-home --uid 10001 fiber \
    && mkdir -p /data/artifacts \
    && chown -R 10001:10001 /data/artifacts
USER 10001
WORKDIR /home/fiber
ENV FIBER_ARTIFACTS_DIR=/data/artifacts
EXPOSE 8080
# /ready, not /health: it is the probe that means "Postgres answers and the background
# loops are running". Compose overrides the timings; this is for a bare `docker run`.
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=6 \
    CMD curl -sf http://127.0.0.1:8080/ready || exit 1
CMD ["fiber-api"]

# The agent executes pipeline shell, so it ships git (workspace prep) and runs as a
# non-root user. Docker steps (`image:`) additionally need the docker CLI and a
# mounted socket — see docs/agents.md.
FROM runtime AS fiber-agent
RUN apt-get update && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 fiber \
    && mkdir -p /data/workspaces && chown fiber:fiber /data/workspaces
COPY --from=builder /app/target/release/fiber-agent /usr/local/bin/fiber-agent
COPY --from=builder /app/target/release/fiber /usr/local/bin/fiber
USER fiber
WORKDIR /home/fiber
ENV FIBER_AGENT_WORKSPACE_DIR=/data/workspaces \
    FIBER_AGENT_USE_DOCKER=false
CMD ["fiber-agent"]
