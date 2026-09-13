.PHONY: help infra infra-minio down api api-s3 agent ui cli build images check fmt fmt-check clippy test test-rust test-ui \
	smoke smoke-authz smoke-pools smoke-artifacts smoke-s3 smoke-compose \
	login validate ready

COMPOSE := docker compose -f deploy/docker-compose.yml
ROOT := $(CURDIR)

help:
	@echo "Fiber DX targets:"
	@echo "  infra         Postgres + Redis (ports 15432 / 16379)"
	@echo "  infra-minio   Also MinIO (19000 API / 19001 console)"
	@echo "  down          Stop Compose services"
	@echo "  build         cargo build workspace + fiber-cli"
	@echo "  check         fmt --check + clippy -D warnings"
	@echo "  test          cargo test --workspace + ui vitest"
	@echo "  images        Build fiber-api + fiber-agent container images"
	@echo "  fmt           cargo fmt"
	@echo "  clippy        cargo clippy"
	@echo "  api           Run fiber-api on :18080 (source scripts/dev-env.sh)"
	@echo "  api-s3        Same with FIBER_USE_S3=1 (needs infra-minio)"
	@echo "  agent         Run fiber-agent (needs FIBER_AGENT_TOKEN)"
	@echo "  ui            pnpm dev in apps/ui (:3100)"
	@echo "  cli           cargo run -p fiber-cli -- …  (ARGS='login')"
	@echo "  login         fiber-cli login (writes ~/.fiber/token)"
	@echo "  validate      Validate examples/fiber.yml"
	@echo "  ready         curl /ready"
	@echo "  smoke         authz + artifacts + pools (needs an agent; pools kills them last)"
	@echo "  smoke-compose Full Compose stack + a pipeline on the containerised agent"
	@echo ""
	@echo "Docs: docs/development.md · docs/roadmap.md · docs/cli.md"
	@echo "CI:   .github/workflows/ci.yml (fmt + clippy + test + build; ui biome + tsc + vitest + build; docker build)"

infra:
	$(COMPOSE) up -d fiber-postgres fiber-redis

infra-minio: infra
	$(COMPOSE) up -d fiber-minio

down:
	$(COMPOSE) down

build:
	cargo build -p fiber-api -p fiber-agent -p fiber-cli

images:
	docker build -f deploy/Dockerfile --target fiber-api -t fiber-api:dev .
	docker build -f deploy/Dockerfile --target fiber-agent -t fiber-agent:dev .

fmt:
	cargo fmt

clippy:
	cargo clippy -p fiber-api -p fiber-agent -p fiber-cli -p fiber-core -p fiber-scheduler -p fiber-durable -- -D warnings

check: fmt-check clippy

test: test-rust test-ui

test-rust:
	cargo test --workspace

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

smoke-s3:
	@echo "Requires: make infra-minio && make api-s3 (in another terminal) + a built fiber-agent"
	python3 scripts/smoke_s3_presign.py

smoke-compose:
	bash scripts/smoke_compose.sh

# Order matters: smoke-pools terminates every fiber-agent on the host before starting
# its own, so anything needing the agent you already have must run before it.
smoke: smoke-authz smoke-artifacts smoke-pools
	@echo "Run smoke-s3 / smoke-compose separately as needed"
