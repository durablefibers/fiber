.PHONY: help infra infra-s3 infra-minio down api api-s3 agent ui cli build images check fmt fmt-check clippy deny test test-rust test-ui \
	smoke smoke-authz smoke-pools smoke-artifacts smoke-concurrency smoke-s3 smoke-compose \
	login validate ready

# deploy/docker-compose.yml has no working default for the four credentials — a
# deployment must put them in deploy/.env, and `docker compose config` fails until it
# does. A dev box is not a deployment: these services publish on 127.0.0.1 only, and
# scripts/dev-env.sh already assumes these throwaway values, so the make targets supply
# them here rather than making every contributor write a deploy/.env.
DEV_CREDS := FIBER_POSTGRES_PASSWORD=$${FIBER_POSTGRES_PASSWORD:-fiber} \
	FIBER_REDIS_PASSWORD=$${FIBER_REDIS_PASSWORD:-fiber} \
	FIBER_S3_ACCESS_KEY=$${FIBER_S3_ACCESS_KEY:-fiber} \
	FIBER_S3_SECRET_KEY=$${FIBER_S3_SECRET_KEY:-fiberfiber} \
	FIBER_ADMIN_PASSWORD=$${FIBER_ADMIN_PASSWORD:-fiber}
# ...but a deploy/.env, if there is one, is the operator's own and wins outright:
# environment beats an env-file in Compose, so injecting these would silently override
# it and Redis would come back up on a password their host API does not have. Anchored
# on this makefile's directory, not the working directory: `make -C` (or a target run
# from a subdirectory) would otherwise miss the file and do exactly that.
HERE := $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
COMPOSE := $(if $(wildcard $(HERE)/deploy/.env),,$(DEV_CREDS)) docker compose -f $(HERE)/deploy/docker-compose.yml
ROOT := $(CURDIR)

# THE gate, defined once. Both .github/workflows/ci.yml and .github/workflows/release.yml
# run `make check`; nothing in either spells cargo out, so there is no second version of
# the gate to drift from this one.
#
#  --workspace     all seven crates, including fiber-proto, which the old CI list and the
#                  old Makefile list each managed to omit at different times
#  --all-targets   lints tests, benches and examples, not just the libraries: a warning
#                  in a #[cfg(test)] module used to pass locally and fail in CI
#  --locked        refuses to silently rewrite Cargo.lock, so a Cargo.toml edit without
#                  the matching lockfile fails at the gate instead of in `docker`
CARGO_GATE_FLAGS := --workspace --all-targets --locked

help:
	@echo "Fiber DX targets:"
	@echo "  infra         Postgres + Redis (ports 15432 / 16379)"
	@echo "  infra-s3      Also the S3 store, RustFS (19000 API / 19001 console)"
	@echo "  down          Stop Compose services"
	@echo "  build         cargo build workspace + fiber-cli"
	@echo "  check         THE gate: fmt --check + clippy --workspace --all-targets --locked -D warnings"
	@echo "  deny          cargo deny check (advisories, licences, sources) — needs cargo-deny"
	@echo "  test          cargo test --workspace --locked + ui vitest"
	@echo "  test-rust     cargo test --workspace --locked"
	@echo "  test-ui       apps/ui vitest"
	@echo "  images        Build fiber-api + fiber-agent container images"
	@echo "  fmt           cargo fmt"
	@echo "  clippy        cargo clippy"
	@echo "  api           Run fiber-api on :18080 (source scripts/dev-env.sh)"
	@echo "  api-s3        Same with FIBER_USE_S3=1 (needs infra-s3)"
	@echo "  agent         Run fiber-agent (needs FIBER_AGENT_TOKEN)"
	@echo "  ui            pnpm dev in apps/ui (:3100)"
	@echo "  cli           cargo run -p fiber-cli -- …  (ARGS='login')"
	@echo "  login         fiber-cli login (writes ~/.fiber/token)"
	@echo "  validate      Validate examples/fiber.yml"
	@echo "  ready         curl /ready"
	@echo "  smoke         authz + artifacts + concurrency + pools (needs an agent; pools kills them last)"
	@echo "  smoke-authz / smoke-artifacts / smoke-concurrency / smoke-pools   one scenario each"
	@echo "  smoke-s3      S3 presign path (needs infra-s3 + api-s3)"
	@echo "  smoke-compose Full Compose stack + a pipeline on the containerised agent"
	@echo ""
	@echo "Docs: docs/development.md · docs/roadmap.md · docs/cli.md"
	@echo "CI:   .github/workflows/ci.yml — check (this make check), deny, ui, docker, smoke, smoke-host"

infra:
	$(COMPOSE) up -d fiber-postgres fiber-redis

# The S3 store sits behind the `s3` Compose profile: artifacts default to the local
# filesystem, and only `make api-s3` / `make smoke-s3` need the object store.
infra-s3: infra
	$(COMPOSE) --profile s3 up -d fiber-s3

# The old name, from when the store was MinIO.
infra-minio: infra-s3

down:
	$(COMPOSE) down

build:
	cargo build --locked -p fiber-api -p fiber-agent -p fiber-cli

images:
	docker build -f deploy/Dockerfile --target fiber-api -t fiber-api:dev .
	docker build -f deploy/Dockerfile --target fiber-agent -t fiber-agent:dev .

fmt:
	cargo fmt

clippy:
	cargo clippy $(CARGO_GATE_FLAGS) -- -D warnings

check: fmt-check clippy

# Supply chain. Not folded into `check`: it downloads the RustSec advisory database, and
# `make check` has to work offline. CI runs the same thing in the `deny` job.
deny:
	@command -v cargo-deny >/dev/null || { echo "cargo install cargo-deny --locked"; exit 1; }
	cargo deny check

test: test-rust test-ui

test-rust:
	cargo test --workspace --locked

test-ui:
	cd apps/ui && pnpm test

fmt-check:
	cargo fmt --check

api:
	@set -a; . scripts/dev-env.sh; set +a; \
	cargo run -p fiber-api

api-s3:
	@set -a; FIBER_USE_S3=1 . scripts/dev-env.sh; set +a; \
	cargo run -p fiber-api

agent:
	@test -n "$${FIBER_AGENT_TOKEN}" || { echo "set FIBER_AGENT_TOKEN (fiber agents create …)"; exit 1; }
	@set -a; . scripts/dev-env.sh; set +a; \
	case "$$FIBER_API_URL" in \
	  https://*) export FIBER_API_URL="wss://$${FIBER_API_URL#https://}" ;; \
	  http://*)  export FIBER_API_URL="ws://$${FIBER_API_URL#http://}" ;; \
	  ws://*|wss://*) ;; \
	  *) export FIBER_API_URL="ws://127.0.0.1:18080" ;; \
	esac; \
	cargo run -p fiber-agent

ui:
	cd apps/ui && pnpm install && VITE_FIBER_API_URL=http://127.0.0.1:18080 pnpm dev

cli:
	cargo run -p fiber-cli -- $(ARGS)

login:
	cargo run -p fiber-cli -- login

validate:
	cargo run -p fiber-cli -- validate examples/fiber.yml

ready:
	@curl -sf http://127.0.0.1:18080/ready | python3 -m json.tool

smoke-authz:
	python3 scripts/smoke_authz_agents.py

smoke-pools:
	python3 scripts/smoke_agent_pools.py

smoke-artifacts:
	python3 scripts/smoke_artifacts.py

smoke-concurrency:
	python3 scripts/smoke_concurrency.py

smoke-s3:
	@echo "Requires: make infra-s3 && make api-s3 (in another terminal) + a built fiber-agent"
	python3 scripts/smoke_s3_presign.py

smoke-compose:
	bash scripts/smoke_compose.sh

# Order matters: smoke-pools terminates every fiber-agent on the host before starting
# its own, so anything needing the agent you already have must run before it.
smoke: smoke-authz smoke-artifacts smoke-concurrency smoke-pools
	@echo "Run smoke-s3 / smoke-compose separately as needed"
