# The published agent plus the Docker CLI, for pipelines with `image:` steps. Built locally
# by the `agent-docker` Compose profile (deploy/docker-compose.yml), so it always matches
# FIBER_VERSION without a separate published image.
#
# Runs as root on purpose: it is handed the Docker socket, which is root on the host
# whatever user the agent claims to be. Only run it where that is acceptable.
ARG FIBER_VERSION=latest
FROM docker:29-cli AS cli

FROM ghcr.io/durablefibers/fiber-agent:${FIBER_VERSION}
USER root
COPY --from=cli /usr/local/bin/docker /usr/local/bin/docker
