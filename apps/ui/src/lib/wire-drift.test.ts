/**
 * Drift guard for the hand-mirrored wire types.
 *
 * `src/lib/api.ts` mirrors the Rust types by hand — there is no codegen, so adding a
 * field on one side and not the other is silent until runtime (CLAUDE.md, "update both
 * sides together"). These tests read the Rust source and fail when it grows a field, a
 * status or an event the TypeScript does not know about.
 *
 * Three crates reach the browser, not one: `fiber-proto` (the pipeline definition and
 * the `/ws/runs/{id}` events), `fiber-core`'s `models.rs` (everything the REST handlers
 * return), and `fiber-durable`'s `types.rs` (the Fibers page). All three are read here.
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
const MODELS = repoFile("crates/fiber-core/src/models.rs")
const DURABLE = repoFile("crates/fiber-durable/src/types.rs")
const API_TS = repoFile("apps/ui/src/lib/api.ts")
const RUN_PAGE = repoFile("apps/ui/src/routes/p.$projectId.runs.$runId.tsx")

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

/**
 * Wire tags of a `#[serde(tag = "type")]` enum whose variants carry fields.
 *
 * `rustEnumWireNames` only sees unit variants (`Foo,`); these close with `},`.
 */
function rustEnumTags(source: string, name: string): string[] {
  const start = source.indexOf(`pub enum ${name} {`)
  if (start === -1) throw new Error(`enum ${name} not found in the Rust source`)
  const open = source.indexOf("{", start)
  const body = source.slice(open + 1, source.indexOf("\n}", open))
  const tags: string[] = []
  let depth = 0
  for (const line of body.split("\n")) {
    const trimmed = line.trim()
    if (depth === 0) {
      const variant = trimmed.match(/^([A-Z][A-Za-z0-9]*)\s*\{/)
      if (variant) tags.push(variant[1])
    }
    depth += (line.match(/\{/g) ?? []).length
    depth -= (line.match(/\}/g) ?? []).length
  }
  if (tags.length === 0) throw new Error(`no variants parsed from ${name}`)
  return tags.map((v) => v.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase())
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

describe("the run pipeline's concurrency block", () => {
  it("mirrors every field of the Rust ConcurrencyConfig", () => {
    // Added in 0.6.0 and missed by this file until the second audit: the pipeline
    // editor reads `group` and `cancel_in_progress` off a type nothing checked.
    const rust = rustStructFields(PROTO, "ConcurrencyConfig")
    const ts = tsTypeProperties(API_TS, "ConcurrencyConfig")
    const missing = rust.filter((f) => !ts.has(f))
    expect(
      missing,
      `fiber-proto ConcurrencyConfig has fields api.ts does not mirror: ${missing.join(", ")}`
    ).toEqual([])
  })
})

describe("the run-stream events", () => {
  // The run page switches on these as bare strings, so a renamed variant is a frame
  // silently dropped by `JSON.parse` and a log view that stops updating.
  const KNOWN_EVENTS = [
    "run_updated",
    "step_updated",
    "log",
    "log_batch",
    "resync",
  ]

  it("covers every RunEvent variant in fiber-proto", () => {
    expect(rustEnumTags(PROTO, "RunEvent").sort()).toEqual(
      [...KNOWN_EVENTS].sort()
    )
  })

  it("is handled in the run page under exactly those tags", () => {
    for (const tag of rustEnumTags(PROTO, "RunEvent")) {
      expect(
        RUN_PAGE.includes(`"${tag}"`),
        `the run page never matches msg.type === "${tag}"`
      ).toBe(true)
    }
  })

  it("mirrors every field of a batch's lines", () => {
    // The viewer resumes from `id` after a resync; losing it here would make the
    // catch-up fetch ask for the wrong range.
    const rust = rustStructFields(PROTO, "LogEventLine")
    expect(rust).toContain("id")
    for (const field of rust) {
      expect(
        RUN_PAGE.includes(field) || field === "at" || field === "seq",
        `the run page ignores LogEventLine.${field}`
      ).toBe(true)
    }
  })
})

describe("the durable fiber types", () => {
  // Same reasoning as the step and run statuses: a new variant should make whoever
  // added it decide how the Fibers page renders it.
  const KNOWN_FIBER = [
    "pending",
    "running",
    "suspended",
    "completed",
    "failed",
    "cancelled",
  ]

  it("covers every FiberStatus in fiber-durable", () => {
    expect(rustEnumWireNames(DURABLE, "FiberStatus").sort()).toEqual(
      [...KNOWN_FIBER].sort()
    )
  })

  it("mirrors every field of FiberRecord and its state", () => {
    const rust = [
      ...rustStructFields(DURABLE, "FiberRecord"),
      ...rustStructFields(DURABLE, "FiberState"),
    ]
    const ts = tsTypeProperties(API_TS, "DurableFiber")
    const missing = rust.filter((f) => !ts.has(f))
    expect(
      missing,
      `fiber-durable types have fields api.ts does not mirror: ${missing.join(", ")}`
    ).toEqual([])
  })
})

/**
 * What `api.ts` mirrors out of `fiber-core/src/models.rs` — the REST payloads, which
 * this file never read before.
 *
 * A field listed in `unmirrored` is one the UI deliberately does not carry, with the
 * reason. Anything else new on the Rust side fails, which is the point: the decision
 * gets made rather than discovered.
 */
const MODEL_MIRRORS: {
  rust: string
  ts: string
  unmirrored?: Record<string, string>
}[] = [
  { rust: "Project", ts: "Project" },
  {
    rust: "Pipeline",
    ts: "Pipeline",
    unmirrored: {
      last_scheduled_at: "scheduler bookkeeping; the UI shows runs, not ticks",
      next_due_at: "scheduler bookkeeping",
    },
  },
  { rust: "ProjectMember", ts: "ProjectMember" },
  {
    rust: "Run",
    ts: "Run",
    unmirrored: {
      head_sha: "no commit detail on the run page yet",
      head_ref: "no commit detail on the run page yet",
      pr_number: "no commit detail on the run page yet",
      repo_full_name: "no commit detail on the run page yet",
      untrusted: "no fork-PR badge on the run page yet",
    },
  },
  { rust: "StepRun", ts: "StepRun" },
  { rust: "StepAttempt", ts: "StepAttempt" },
  { rust: "LogLine", ts: "LogLine" },
  {
    rust: "Agent",
    ts: "Agent",
    unmirrored: {
      token_hash: "a credential digest; it must never reach a browser",
    },
  },
  {
    rust: "Artifact",
    ts: "Artifact",
    unmirrored: {
      path: "storage key; the UI downloads by artifact id",
    },
  },
  { rust: "PublicUser", ts: "PublicUser" },
  { rust: "ProjectSecretMeta", ts: "SecretMeta" },
]

describe("the REST payloads api.ts mirrors from fiber-core", () => {
  for (const { rust, ts, unmirrored } of MODEL_MIRRORS) {
    it(`mirrors every field of ${rust}`, () => {
      const fields = rustStructFields(MODELS, rust)
      const mirrored = tsTypeProperties(API_TS, ts)
      const missing = fields.filter(
        (f) => !mirrored.has(f) && !(unmirrored && f in unmirrored)
      )
      expect(
        missing,
        `models.rs ${rust} has fields api.ts neither mirrors nor excuses: ${missing.join(", ")}`
      ).toEqual([])
    })
  }

  it("does not excuse a field that is in fact mirrored", () => {
    // Keeps the list above honest: a stale excuse would hide the next real change.
    for (const { rust, ts, unmirrored } of MODEL_MIRRORS) {
      const fields = new Set(rustStructFields(MODELS, rust))
      const mirrored = tsTypeProperties(API_TS, ts)
      for (const field of Object.keys(unmirrored ?? {})) {
        expect(fields.has(field), `${rust}.${field} no longer exists`).toBe(
          true
        )
        expect(
          mirrored.has(field),
          `${rust}.${field} is mirrored after all; drop its excuse`
        ).toBe(false)
      }
    }
  })
})
