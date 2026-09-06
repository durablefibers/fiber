# syntax=docker/dockerfile:1
# One build for every Rust binary, selected with `--target`:
#   docker build -f deploy/Dockerfile --target fiber-api   .
#   docker build -f deploy/Dockerfile --target fiber-agent .
# cargo-chef gives the dependency graph its own layer, so editing crates/ does not
# rebuild aws-sdk-s3 / sqlx / opentelemetry every time.
FROM rust:1.97-bookworm AS chef
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

FROM runtime AS fiber-api
COPY --from=builder /app/target/release/fiber-api /usr/local/bin/fiber-api
RUN mkdir -p /data/artifacts
ENV FIBER_ARTIFACTS_DIR=/data/artifacts
EXPOSE 8080
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
