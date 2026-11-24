import { readFileSync } from "node:fs"
import {
  artifactPathOk,
  byteLength,
  commitShaOk,
  gitRefOk,
  imageReferenceOk,
  repoUrlOk,
  shellOk,
  truncateLogLine,
  workingDirectoryOk,
} from "@fiber/proto"
import { describe, expect, test } from "vitest"
import {
  compileDefinition,
  DagError,
  DefinitionError,
  evalIf,
  nextCron,
  parseCron,
  parsePipelineYaml,
  parseRole,
  pathsAllow,
  validateGlobs,
  validateIf,
  validateKey,
} from "../src/index.ts"

/** Emitted by `crates/fiber-core/examples/emit_vectors.rs`; the Rust server is the oracle. */
const v = JSON.parse(
  readFileSync(new URL("../../../conformance/vectors/core.json", import.meta.url), "utf8")
)
const plain = (x: unknown): unknown => JSON.parse(JSON.stringify(x))
const bits = (...b: boolean[]): string => b.map((x) => (x ? "1" : "0")).join("")

/** Report every mismatch, so a drifted rule shows its whole shape. */
// biome-ignore lint/suspicious/noExplicitAny: vectors are untyped JSON
type Case = any
function compare(cases: { in: unknown }[], run: (c: Case) => unknown, want: (c: Case) => unknown) {
  expect(cases.length).toBeGreaterThan(0)
  const bad = cases.flatMap((c) => {
    const got = plain(run(c))
    const exp = plain(want(c))
    return JSON.stringify(got) === JSON.stringify(exp) ? [] : [{ in: c.in, got, want: exp }]
  })
  expect(bad.slice(0, 12), `${bad.length} of ${cases.length} differ`).toEqual([])
}
/** Our own sentences must match; text produced by serde or the YAML parser need not. */
const ours = (e: string | undefined): boolean =>
  e !== undefined &&
  /unknown field|keys must be|merge keys|every step id|step id cannot|remove `id:`|steps: must be/.test(
    e
  )

describe("golden vectors", () => {
  test("yaml parses, compiles and fails as the oracle does", () => {
    const sorted = (x: unknown): unknown =>
      Array.isArray(x)
        ? x.map(sorted)
        : x && typeof x === "object"
          ? Object.fromEntries(
              Object.entries(x)
                .sort(([a], [b]) => (a < b ? -1 : 1))
                .map(([k, y]) => [k, sorted(y)])
            )
          : x
    compare(
      v.yaml,
      (c) => {
        let def: ReturnType<typeof parsePipelineYaml>
        try {
          def = parsePipelineYaml(c.in)
        } catch (e) {
          if (!(e instanceof DefinitionError)) throw e
          return { stage: "parse", error: ours(c.error) ? e.message : "*" }
        }
        try {
          return { stage: "ok", def: sorted(def), dag: sorted(compileDefinition(def)) }
        } catch (e) {
          if (!(e instanceof DagError)) throw e
          return { stage: "compile", def: sorted(def), error: e.message }
        }
      },
      (c) =>
        c.stage === "parse"
          ? { stage: "parse", error: ours(c.error) ? c.error : "*" }
          : c.stage === "ok"
            ? { stage: "ok", def: sorted(c.def), dag: sorted(c.dag) }
            : { stage: "compile", def: sorted(c.def), error: c.error }
    )
  })
  test("step conditions", () =>
    compare(
      v.if,
      (c) => [
        evalIf(c.in, true, v.ifEnv),
        evalIf(c.in, false, v.ifEnv),
        validateIf(c.in),
        validateKey(c.in, v.ifEnv),
        validateKey(c.in, []),
      ],
      (c) => [c.t, c.f, c.shape, c.key, c.nokey]
    ))
  test("conditions read the FIBER_MATRIX_ alias", () =>
    compare(
      v.alias,
      (c) => [evalIf(c.in, true, v.aliasEnv), validateKey(c.in, v.aliasEnv)],
      (c) => [c.t, c.key]
    ))
  test("cron grammar and next occurrence", () =>
    compare(
      v.cron,
      (c) => {
        let ok = true
        try {
          parseCron(c.in)
        } catch {
          ok = false
        }
        return [ok, v.times.map((t: number) => nextCron(c.in, t))]
      },
      (c) => [c.ok, c.next]
    ))
  test("globs", () =>
    compare(
      v.glob,
      (c) => [
        bits(...v.paths.map((p: string) => pathsAllow([p], [c.in], []))),
        validateGlobs("f", [c.in]),
      ],
      (c) => [c.m, c.error?.replace(/: error parsing glob '[\s\S]*?': /, ": ")]
    ))
  test("validators", () =>
    compare(
      v.validate,
      ({ in: s }) =>
        bits(
          imageReferenceOk(s),
          repoUrlOk(s),
          commitShaOk(s),
          gitRefOk(s),
          workingDirectoryOk(s),
          shellOk(s),
          artifactPathOk(s)
        ),
      (c) => c.m
    ))
  test("log line truncation counts UTF-8 bytes", () =>
    compare(
      v.truncate.map((c: Case) => ({ ...c, in: `${c.unit} x${c.n}` })),
      (c) => {
        const out = truncateLogLine(c.unit.repeat(c.n))
        return [byteLength(out), Array.from(out).length]
      },
      (c) => [c.bytes, c.chars]
    ))
  test("roles", () =>
    compare(
      v.roles,
      (c) => parseRole(c.in),
      (c) => c.out
    ))
})
