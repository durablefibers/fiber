---
name: fiber-migration
description: Procedure for changing the Fiber Postgres schema — writing the next numbered sqlx migration, the matching FromRow model, and the runtime store query, with the rolling-deploy safety rules. Use whenever a change needs a new table, column, index, or constraint, or when a migration fails to apply at fiber-api boot.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout, Rust stable, and a reachable Postgres (make infra).
metadata:
  author: durablefibers
  version: "1.0"
---

# Changing the Fiber schema

## Migrations already applied

```!
ls crates/fiber-core/migrations/
```

The next file is the highest number plus one: `00N_<topic>.sql`.

## The rule

`sqlx::migrate!("./migrations")` runs at `fiber-api` boot and **checksums every file**. Editing one that any database has already applied makes that database refuse to start. There is no amend — only the next file. The toolkit's `guard-edits` and `guard-bash` hooks block edits to committed migrations for this reason.

## Rolling-deploy safety

Migrations apply when the new image boots, while the **old image is still serving**. Both must work against the new schema:

- New columns are nullable or have a `DEFAULT`. Never bare `NOT NULL` on a populated table.
- No rename or type narrowing in one step. Add → backfill → switch reads → drop in a later release.
- Drop a column at least one release after code stopped reading it.
- Large-table indexes: consider `CREATE INDEX CONCURRENTLY` (cannot run inside a transaction).

## The three files

1. **`crates/fiber-core/migrations/00N_<topic>.sql`** — the DDL.
2. **`crates/fiber-core/src/models.rs`** — the `#[derive(sqlx::FromRow)]` struct. Types must match columns exactly: nullable → `Option<T>`, `timestamptz` → `DateTime<Utc>`, `uuid` → `Uuid`, `jsonb` → `serde_json::Value`.
3. **`crates/fiber-core/src/store.rs`** — queries as runtime `sqlx::query_as::<_, T>` with binds. **Never `sqlx::query!`**: it needs a live `DATABASE_URL` at compile time and CI has no database.

Because queries are runtime-checked, a mismatch between column and struct is a **runtime** failure that `cargo build` will not catch. Verification means booting against a real database.

## Cascade and retention

Deletes cascade from `runs` to `step_runs`, `log_lines`, `step_attempts`. `003_retention.sql` adds the indexes the retention sweep uses. If your new table references a run, decide its `ON DELETE` behavior explicitly and check `store.rs::delete_runs` and `fiber-api/src/retention.rs` — a table that does not cascade will accumulate orphans forever.

## Verify

```bash
make infra                 # Postgres up
make check                 # fmt + clippy
make api                   # watch the boot log for the migration line
```

Then exercise the new query for real. Report the boot log line. See [references/patterns.md](references/patterns.md) for worked DDL patterns and the backfill sequence.
