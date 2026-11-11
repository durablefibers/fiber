# Fiber agent notes

**[CLAUDE.md](CLAUDE.md) is the guide for any coding agent working here**, whatever its
vendor: commands, architecture, the test strategy, and the gotchas. Read it first. The
rules below are the ones that produce a defect which compiles and passes review by
inspection; the reasoning for each is in
[.claude/skills/fiber-conventions/SKILL.md](.claude/skills/fiber-conventions/SKILL.md).

1. Product prefix is `fiber` / `FIBER_*` everywhere — never `df` or `durablefibers`
   ([.claude/rules/naming.md](.claude/rules/naming.md)).
2. `make check` is the gate (fmt + clippy `-D warnings`), and `make test` the suite. Use
   Make targets and `scripts/dev-env.sh`, not ad-hoc shell
   ([.claude/rules/dx.md](.claude/rules/dx.md)).
3. CI steps and durable fibers are at-least-once. Anything on the execution path must
   survive running twice.
4. A run executes its definition snapshot, never the live pipeline row.
5. `step_attempts` and `log_lines` are append-only.
6. Migrations are append-only: add the next numbered file in
   `crates/fiber-core/migrations/`, never edit a committed one.
7. All SQL is runtime `sqlx` — no `query!` macros — so a column mismatch is a runtime
   error. Run schema-touching changes against a real Postgres.
8. Every project-scoped HTTP handler goes through `crates/fiber-api/src/access.rs`.
9. Wire types in `fiber-proto` have four consumers, one of them hand-mirrored
   TypeScript (`apps/ui/src/lib/api.ts`). Change them together.

Hazards: never kill agents by argv pattern (it matches your own parent shell — kill by
PID); never remove the Compose volumes to "reset"; `data/`, `target/`, `apps/ui/dist/`
and `apps/ui/src/routeTree.gen.ts` are generated.

Docs index: [docs/README.md](docs/README.md). Roadmap: [docs/roadmap.md](docs/roadmap.md).
