.PHONY: help infra infra-minio down api api-s3 agent web cli build check fmt fmt-check clippy \
	dogfood dogfood-authz dogfood-pools dogfood-artifacts dogfood-s3 \
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
	@echo "  fmt           cargo fmt"
	@echo "  clippy        cargo clippy"
	@echo "  api           Run fiber-api on :18080 (source scripts/dev-env.sh)"
	@echo "  api-s3        Same with FIBER_USE_S3=1 (needs infra-minio)"
	@echo "  agent         Run fiber-agent (needs FIBER_AGENT_TOKEN)"
	@echo "  web           pnpm dev in apps/web (:3100)"
	@echo "  cli           cargo run -p fiber-cli -- …  (ARGS='login')"
	@echo "  login         fiber-cli login (writes ~/.fiber/token)"
	@echo "  validate      Validate examples/fiber.yml"
	@echo "  ready         curl /ready"
	@echo "  dogfood       All smoke scripts (authz, pools, artifacts, s3)"
	@echo ""
	@echo "Docs: docs/development.md · docs/roadmap.md · docs/cli.md"
	@echo "CI:   .github/workflows/ci.yml (fmt + clippy + build + web tsc)"

infra:
	$(COMPOSE) up -d fiber-postgres fiber-redis

infra-minio: infra
	$(COMPOSE) up -d fiber-minio

down:
	$(COMPOSE) down

build:
	cargo build -p fiber-api -p fiber-agent -p fiber-cli

fmt:
	cargo fmt

clippy:
	cargo clippy -p fiber-api -p fiber-agent -p fiber-cli -p fiber-core -p fiber-scheduler -p fiber-durable -- -D warnings

check: fmt-check clippy

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

web:
	cd apps/web && pnpm install && VITE_FIBER_API_URL=http://127.0.0.1:18080 pnpm dev

cli:
	cargo run -p fiber-cli -- $(ARGS)

login:
	cargo run -p fiber-cli -- login

validate:
	cargo run -p fiber-cli -- validate examples/fiber.yml

ready:
	@curl -sf http://127.0.0.1:18080/ready | python3 -m json.tool

dogfood-authz:
	python3 scripts/dogfood_authz_agents.py

dogfood-pools:
	python3 scripts/dogfood_agent_pools.py

dogfood-artifacts:
	python3 scripts/dogfood_artifacts.py

dogfood-s3:
	@echo "Requires: make infra-minio && make api-s3 (in another terminal) + a built fiber-agent"
	python3 scripts/dogfood_s3_presign.py

dogfood: dogfood-authz dogfood-pools dogfood-artifacts
	@echo "Run dogfood-s3 separately with MinIO + api-s3"
