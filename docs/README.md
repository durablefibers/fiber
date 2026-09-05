# Fiber docs

Self-hosted, canvas-first CI. Rust control plane (`fiber-api`), agents (`fiber-agent`), web UI (`apps/web`), and optional durable background tasks (`fiber-durable`).

| Doc | Contents |
|---|---|
| [Getting started](./getting-started.md) | Local stack, first pipeline, agent |
| [Development](./development.md) | Make targets, ports, `scripts/dev-env.sh` |
| [Architecture](./architecture.md) | Components, data flow, durability model |
| [Pipeline YAML](./pipeline-yaml.md) | `fiber.yml` schema, matrix, `if`, examples |
| [Triggers](./triggers.md) | Push, PR, cron, intervals, path filters |
| [Agents](./agents.md) | Workers, labels, heartbeats, tokens |
| [Artifacts](./artifacts.md) | Upload, restore, S3 presign |
| [Auth & roles](./authz.md) | Login, project membership, secrets |
| [CLI](./cli.md) | `fiber` validate, members, secrets, agents, fibers |
| [Durable fibers](./durable-fibers.md) | Control-plane `step` / `stash` / `sleep` tasks |
| [Configuration](./configuration.md) | Environment variables |
| [HTTP & WebSocket API](./api.md) | Routes and auth |
| [Operations](./operations.md) | Health, retention, OTel, dogfood |
| [Roadmap](./roadmap.md) | Later: GitLab, Vault/OIDC, cloud agents, plugins |

Product overview and quick start also live in the root [README](../README.md). Examples: [`examples/`](../examples/).
