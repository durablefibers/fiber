---
name: fiber-api-endpoint
description: End-to-end procedure for adding or changing an HTTP endpoint in fiber-api — router wiring, role gate, store query, response type, the hand-mirrored TypeScript client, and the docs. Use when adding a route, changing a response shape, or when an endpoint exists in Rust but the UI cannot see it.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout, Rust stable, and pnpm for the web client half.
metadata:
  author: durablefibers
  version: "1.0"
---

# Adding an endpoint to fiber-api

An endpoint is never one file. Doing four of these five steps is the standard way this repo breaks.

## Current router

```!
grep -c 'route(' crates/fiber-api/src/routes.rs
```

## 1. Store method — `crates/fiber-core/src/store.rs`

Add to the single large `impl Store`. Runtime sqlx with binds, returning `anyhow::Result`:

```rust
pub async fn list_widgets(&self, project_id: Uuid) -> Result<Vec<Widget>> {
    Ok(sqlx::query_as::<_, Widget>(
        "SELECT * FROM widgets WHERE project_id = $1 ORDER BY created_at DESC",
    )
    .bind(project_id)
    .fetch_all(&self.pool)
    .await?)
}
```

Never `sqlx::query!` — see the `fiber-conventions` skill for why. If the row type is new, add the `sqlx::FromRow` struct to `models.rs` with field types matching the columns exactly.

## 2. Handler — `crates/fiber-api/src/routes.rs`

Every project-scoped handler gates first, then acts:

```rust
async fn list_widgets(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Vec<Widget>>, ApiError> {
    access::require_project(&state, &user, project_id, ProjectRole::Reader).await?;
    Ok(Json(state.store.list_widgets(project_id).await?))
}
```

Choose the minimum role honestly: reads `Reader`, mutations `Writer` or above, membership and agent administration `Admin`/`Owner`. For ids that are not a project id, resolve through `require_pipeline` / `require_run` so the check and the object come from the same lookup.

Agent-facing endpoints use the `AuthAgent` extractor instead — never accept a session token where an agent token is meant, or the reverse.

## 3. Router wiring

Add the `.route(...)` line in the same block, keeping the existing path style: `/api/projects/{id}/…` for project-scoped collections, `/api/<entity>/{id}` for direct entity access.

## 4. TypeScript client — `apps/web/src/lib/api.ts`

This file hand-mirrors the Rust types. There is no codegen; drift is silent until runtime. Add the type and the fetch function together, matching the serde field names exactly (snake_case as serialized). Auth is the bearer session token; the WS origin derives from `VITE_FIBER_API_URL`.

## 5. Docs — `docs/api.md`

Add the route with method, path, auth (session vs agent token), minimum role, and response shape. `docs/api.md` is expected to be complete; a route missing from it is treated as a documentation bug.

## Verify

```bash
make check                                   # fmt + clippy -D warnings
cd apps/web && pnpm exec tsc --noEmit        # the CI web gate
make ready                                   # API up?
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:18080/api/...
```

Confirm the role gate by calling as a user *without* the role and asserting 403 — a passing happy path proves nothing about authorization.
