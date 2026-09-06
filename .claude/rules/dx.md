# Developer experience

- Prefer **`make help`** targets (`infra`, `api`, `api-s3`, `web`, `agent`, `check`, `test`, `dogfood`) over inventing one-off shell.
- Source **`scripts/dev-env.sh`** (or `FIBER_USE_S3=1`) for host-run processes; see `.env.example`.
- Gate: **`make check`** (fmt + clippy `-D warnings`) and **`make test`**. CI runs the same in `.github/workflows/ci.yml`.
- Docs entry: [docs/development.md](../../docs/development.md). Roadmap: [docs/roadmap.md](../../docs/roadmap.md).
- Kill agents by PID of `./target/debug/fiber-agent` only — never `pkill -f fiber-agent` (matches shells that mention the path).
- Product naming stays `fiber-*` / `FIBER_*` (see [naming.md](./naming.md)).
