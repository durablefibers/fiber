/**
 * Drift guard for the hand-mirrored wire types.
 *
 * `src/lib/api.ts` mirrors the Rust types in `fiber-proto` by hand — there is no codegen,
 * so adding a field on one side and not the other is silent until runtime (CLAUDE.md,
 * "update both sides together"). These tests read the Rust source and fail when it grows
 * a field or a status the TypeScript does not know about.
 *
 * They compare *wire* names, honouring `#[serde(rename = "...")]`, since that is what
 * actually crosses the boundary.
 */
import { existsSync, readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { describe, expect, it } from "vitest"

/** Walk up from the working directory to the workspace root (the one with Cargo.toml). */
function repoRoot(): string {
  let dir = process.cwd()
  while (!existsSync(resolve(dir, "Cargo.toml"))) {
    const parent = dirname(dir)
    if (parent === dir) throw new Error(`no Cargo.toml above ${process.cwd()}`)
    dir = parent
  }
  return dir
}

// Resolved at run time: a `new URL(..., import.meta.url)` here would be rewritten by
// Vite as a static asset import and never reach the filesystem.
const repoFile = (rel: string) => readFileSync(resolve(repoRoot(), rel), "utf8")

const PROTO = repoFile("crates/fiber-proto/src/lib.rs")
const API_TS = repoFile("apps/ui/src/lib/api.ts")

/** Wire field names of a `pub struct` in Rust source, following `serde(rename)`. */
function rustStructFields(source: string, name: string): string[] {
  const start = source.indexOf(`pub struct ${name} {`)
  if (start === -1)
    throw new Error(`struct ${name} not found in the Rust source`)
  const open = source.indexOf("{", start)
  const body = source.slice(open + 1, source.indexOf("\n}", open))

  const fields: string[] = []
  let rename: string | null = null
  for (const line of body.split("\n")) {
    const trimmed = line.trim()
    const renamed = trimmed.match(/#\[serde\([^\])]*rename\s*=\s*"([^"]+)"/)
    if (renamed) rename = renamed[1]
    const field = trimmed.match(/^pub ([a-z_0-9]+)\s*:/)
    if (field) {
      fields.push(rename ?? field[1])
      rename = null
    }
  }
  if (fields.length === 0) throw new Error(`no fields parsed from ${name}`)
  return fields
}

/** Variant names of a snake_case-renamed `pub enum`, as they appear on the wire. */
function rustEnumWireNames(source: string, name: string): string[] {
  const start = source.indexOf(`pub enum ${name} {`)
  if (start === -1) throw new Error(`enum ${name} not found in the Rust source`)
  const open = source.indexOf("{", start)
  const body = source.slice(open + 1, source.indexOf("\n}", open))
  return body
    .split("\n")
    .map((l) => l.trim().match(/^([A-Z][A-Za-z0-9]*)\s*,/)?.[1])
    .filter((v): v is string => Boolean(v))
    .map((v) => v.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase())
}

/** Property names declared anywhere inside an exported TS type, nested objects included. */
function tsTypeProperties(source: string, name: string): Set<string> {
  const start = source.indexOf(`export type ${name} = {`)
  if (start === -1) throw new Error(`type ${name} not found in api.ts`)
  let depth = 0
  let end = start
  for (let i = source.indexOf("{", start); i < source.length; i++) {
    if (source[i] === "{") depth++
    else if (source[i] === "}") {
      depth--
      if (depth === 0) {
        end = i
        break
      }
    }
  }
  const body = source.slice(start, end)
  const props = new Set<string>()
  for (const line of body.split("\n")) {
    const m = line.trim().match(/^([a-z_0-9]+)\??\s*:/)
    if (m) props.add(m[1])
  }
  return props
}

describe("the hand-mirrored pipeline definition", () => {
  it("mirrors every field of the Rust StepDefinition", () => {
    const rust = rustStructFields(PROTO, "StepDefinition")
    const ts = tsTypeProperties(API_TS, "StepDefinition")
    const missing = rust.filter((f) => !ts.has(f))
    expect(
      missing,
      `fiber-proto StepDefinition has fields api.ts does not mirror: ${missing.join(", ")}`
    ).toEqual([])
  })

  it("mirrors every field of the Rust PipelineDefinition and its triggers", () => {
    // The TS type nests workspace and triggers inline, so compare against the union.
    const rust = [
      ...rustStructFields(PROTO, "PipelineDefinition"),
      ...rustStructFields(PROTO, "WorkspaceConfig"),
      ...rustStructFields(PROTO, "PipelineTriggers"),
      ...rustStructFields(PROTO, "PushTrigger"),
      ...rustStructFields(PROTO, "PullRequestTrigger"),
    ]
    const ts = tsTypeProperties(API_TS, "PipelineDefinition")
    const missing = rust.filter((f) => !ts.has(f))
    expect(
      missing,
      `fiber-proto pipeline types have fields api.ts does not mirror: ${missing.join(", ")}`
    ).toEqual([])
  })

  it("reads the renamed fields as their wire names, not their Rust names", () => {
    // Guards the parser itself: `if_expr`/`git_ref` must be seen as `if`/`ref`, or the
    // two tests above would compare the wrong strings and pass vacuously.
    expect(rustStructFields(PROTO, "StepDefinition")).toContain("if")
    expect(rustStructFields(PROTO, "StepDefinition")).not.toContain("if_expr")
    expect(rustStructFields(PROTO, "WorkspaceConfig")).toContain("ref")
  })
})

describe("the statuses the UI knows", () => {
  // Kept as a literal list on purpose: a new Rust variant should fail here and make
  // whoever added it decide how the UI renders it, rather than silently falling through
  // to the default grey.
  const KNOWN_STEP = [
    "pending",
    "queued",
    "running",
    "succeeded",
    "failed",
    "cancelled",
    "skipped",
  ]
  const KNOWN_RUN = ["pending", "running", "succeeded", "failed", "cancelled"]

  it("covers every StepStatus in fiber-proto", () => {
    expect(rustEnumWireNames(PROTO, "StepStatus").sort()).toEqual(
      [...KNOWN_STEP].sort()
    )
  })

  it("covers every RunStatus in fiber-proto", () => {
    expect(rustEnumWireNames(PROTO, "RunStatus").sort()).toEqual(
      [...KNOWN_RUN].sort()
    )
  })
})
