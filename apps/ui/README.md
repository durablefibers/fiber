# apps/ui — Fiber UI

The canvas-first web UI for Fiber. TanStack Start + React 19, React Flow for the
pipeline canvas, shadcn/ui on Tailwind v4, Biome for lint and format.

## Run it

```bash
make ui          # from the repo root: pnpm install + dev server on :3100
```

It talks to `fiber-api` at `VITE_FIBER_API_URL` (default `http://127.0.0.1:18080`),
so start the API first (`make infra && make api`). Login defaults are **admin / fiber**.

## Layout

| Path | What lives there |
|---|---|
| `src/routes/` | File-based routes; `routeTree.gen.ts` is **generated** — never hand-edit |
| `src/components/dag-canvas.tsx` | React Flow canvas, layout, edges |
| `src/components/step-node.tsx` | The step node rendered on the canvas |
| `src/components/app-shell.tsx` | Nav, project switcher, page chrome |
| `src/components/ui/` | shadcn primitives (`npx shadcn@latest add …` lands here) |
| `src/lib/api.ts` | The single typed API client — **hand-mirrors the Rust types** |

`src/lib/api.ts` has no codegen behind it: it mirrors `fiber-proto` by hand, and
`src/lib/wire-drift.test.ts` reads the Rust source to assert the two have not drifted.
Change a wire type on one side and you must change the other.

## Checks

```bash
pnpm check              # Biome (not ESLint/Prettier)
pnpm exec tsc --noEmit  # the hard CI gate
pnpm test               # vitest, jsdom, src/**/*.test.ts(x)
pnpm build
```

CI runs all four. See `docs/development.md` for the full stack.
