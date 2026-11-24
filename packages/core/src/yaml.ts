/**
 * Parse `fiber.yml`. Scalars are resolved here rather than by the YAML library because the
 * schema is strict about types and authors rely on the details: `0777` and `yes` are
 * strings, `1` is a number and therefore not a valid env value, and map order is kept.
 */
import {
  type ConcurrencyConfig,
  compareCodePoints,
  type PipelineDefinition,
  type PipelineTriggers,
  type StepDefinition,
  type WorkspaceConfig,
} from "@fiber/proto"
import { type Document, isAlias, isMap, isScalar, isSeq, parseDocument } from "yaml"
import { auditPipelineKeys } from "./audit.ts"

export type Num = { int: bigint } | { float: number }
export type Value = null | boolean | string | Num | Value[] | Map<Value, Value>
export class DefinitionError extends Error {}

const MAX_NODES = 200_000
const MAX_DEPTH = 128

function plainScalar(s: string): Value {
  if (s === "" || s === "~" || s === "null" || s === "Null" || s === "NULL") return null
  if (s === "true" || s === "True" || s === "TRUE") return true
  if (s === "false" || s === "False" || s === "FALSE") return false
  const body = /^[+-]/.test(s) ? s.slice(1) : s
  if (/^0(x[0-9a-fA-F]+|o[0-7]+|b[01]+)$/.test(body)) {
    const n = BigInt(body)
    return { int: s.startsWith("-") ? -n : n }
  }
  // Leading zeros make it text: a file mode or an id, not a quantity.
  if (/^0[0-9]/.test(body) && /^[0-9]+$/.test(body)) return s
  if (/^[0-9]+$/.test(body)) return { int: BigInt(s.startsWith("+") ? body : s) }
  if (/^\.(inf|Inf|INF)$/.test(body)) return { float: s.startsWith("-") ? -Infinity : Infinity }
  if (s === ".nan" || s === ".NaN" || s === ".NAN") return { float: Number.NaN }
  if (/^([0-9]+\.?[0-9]*|\.[0-9]+)([eE][+-]?[0-9]+)?$/.test(body)) {
    const n = Number(s)
    if (Number.isFinite(n)) return { float: n }
  }
  return s
}

export function parseYaml(text: string): Value {
  let doc: Document
  try {
    doc = parseDocument(text, { schema: "failsafe", merge: false, uniqueKeys: true })
  } catch (e) {
    throw new DefinitionError((e as Error).message)
  }
  const err = doc.errors[0]
  if (err) throw new DefinitionError(err.message.split("\n")[0] ?? "invalid YAML")
  let nodes = 0
  const conv = (node: unknown, depth: number): Value => {
    if (++nodes > MAX_NODES) throw new DefinitionError("YAML expands to too many values")
    if (depth > MAX_DEPTH) throw new DefinitionError("YAML is nested too deeply")
    if (node === null || node === undefined) return null
    if (isAlias(node)) {
      const target = node.resolve(doc)
      if (!target) throw new DefinitionError(`unknown anchor \`${node.source}\``)
      return conv(target, depth + 1)
    }
    if (isScalar(node)) {
      const s = typeof node.value === "string" ? node.value : String(node.value ?? "")
      return node.type === "PLAIN" && !node.tag ? plainScalar(s) : s
    }
    if (isSeq(node)) return node.items.map((n) => conv(n, depth + 1))
    if (isMap(node)) {
      const m = new Map<Value, Value>()
      for (const p of node.items) m.set(conv(p.key, depth + 1), conv(p.value, depth + 1))
      return m
    }
    throw new DefinitionError("unsupported YAML node")
  }
  return conv(doc.contents, 0)
}

/** JSON submitted to the API, as the same value type. */
export function fromJson(v: unknown): Value {
  if (v === null || v === undefined) return null
  if (typeof v === "number") return Number.isInteger(v) ? { int: BigInt(v) } : { float: v }
  if (typeof v === "string" || typeof v === "boolean") return v
  if (Array.isArray(v)) return v.map(fromJson)
  return new Map(Object.entries(v as object).map(([k, x]) => [k, fromJson(x)]))
}

export function kindOf(v: Value): string {
  if (v === null) return "null"
  if (typeof v === "boolean") return "a boolean"
  if (typeof v === "string") return "a string"
  if (Array.isArray(v)) return "a list"
  return v instanceof Map ? "a map" : "a number"
}

// --- decoding -----------------------------------------------------------------------------

type Obj = Map<Value, Value>
const fail = (path: string, want: string, got: Value): never => {
  throw new DefinitionError(`${path}: expected ${want}, found ${kindOf(got)}`)
}
/** An empty value reads as an empty map, so `a:` with no body is a step with no fields. */
const obj = (v: Value, path: string): Obj => {
  if (v === null) return new Map()
  return v instanceof Map ? v : fail(path, "a map", v)
}
const str = (v: Value, path: string): string =>
  typeof v === "string" ? v : fail(path, "a string", v)
const optStr = (m: Obj, k: string, p: string): string | null => {
  const v = m.get(k)
  return v === undefined || v === null ? null : str(v, `${p}.${k}`)
}
const strs = (v: Value, path: string): string[] =>
  Array.isArray(v) ? v.map((x, i) => str(x, `${path}[${i}]`)) : fail(path, "a list", v)
const list = (m: Obj, k: string, p: string): string[] => {
  const v = m.get(k)
  // An empty `needs:` reads as an empty list.
  return v === undefined || v === null ? [] : strs(v, `${p}.${k}`)
}
const bool = (m: Obj, k: string, p: string): boolean => {
  const v = m.get(k)
  if (v === undefined) return false
  return typeof v === "boolean" ? v : fail(`${p}.${k}`, "a boolean", v)
}
const u32 = (m: Obj, k: string, p: string): number | null => {
  const v = m.get(k)
  if (v === undefined || v === null) return null
  if (typeof v === "object" && "int" in v && v.int >= 0n && v.int <= 0xffffffffn)
    return Number(v.int)
  return fail(`${p}.${k}`, "a whole number from 0 to 4294967295", v)
}
/** Sorted by key, with no prototype so a key such as `__proto__` is just a key. */
function record<T>(v: Value, path: string, each: (x: Value, p: string) => T): Record<string, T> {
  const out: Record<string, T> = Object.create(null)
  if (v === null) return out
  const entries = [...obj(v, path)].map(([k, x]) => [str(k, path), x] as const)
  entries.sort((a, b) => compareCodePoints(a[0], b[0]))
  for (const [k, x] of entries) out[k] = each(x, `${path}.${k}`)
  return out
}

function step(m: Obj, id: string, path: string, name: string): StepDefinition {
  const matrix = m.get("matrix")
  const secrets = m.get("secrets")
  return {
    id,
    name,
    needs: list(m, "needs", path),
    run: optStr(m, "run", path),
    image: optStr(m, "image", path),
    labels: list(m, "labels", path),
    retries: u32(m, "retries", path) ?? 0,
    env: record(m.get("env") ?? null, `${path}.env`, str),
    working_directory: optStr(m, "working_directory", path),
    shell: optStr(m, "shell", path),
    continue_on_error: bool(m, "continue_on_error", path),
    artifacts: list(m, "artifacts", path),
    matrix: matrix === undefined || matrix === null ? null : record(matrix, `${path}.matrix`, strs),
    if: optStr(m, "if", path),
    timeout_minutes: u32(m, "timeout_minutes", path),
    secrets: secrets === undefined || secrets === null ? null : strs(secrets, `${path}.secrets`),
  }
}

function steps(v: Value): StepDefinition[] {
  if (Array.isArray(v))
    return v.map((x, i) => {
      const p = `steps[${i}]`
      const m = obj(x, p)
      return step(
        m,
        str(m.get("id") ?? null, `${p}.id`),
        p,
        str(m.get("name") ?? null, `${p}.name`)
      )
    })
  if (v instanceof Map)
    return [...v].map(([k, x]) => {
      const id = typeof k === "string" ? k : ""
      const m = obj(x, `steps.${id}`)
      return step(m, id, `steps.${id}`, optStr(m, "name", `steps.${id}`) ?? id)
    })
  throw new DefinitionError(
    `steps: must be a list of steps or a map of id to step, not ${kindOf(v)}`
  )
}

const opt = <T>(m: Obj, k: string, f: (o: Obj) => T): T | null => {
  const v = m.get(k)
  return v === undefined || v === null ? null : f(obj(v, k))
}

/** Decode without the unknown-key audit: for definitions that are already stored. */
export function decodeDefinition(root: Value): PipelineDefinition {
  const m = obj(root, "pipeline")
  if (!m.has("steps")) throw new DefinitionError("pipeline: missing field `steps`")
  return {
    name: str(m.get("name") ?? null, "name"),
    env: record(m.get("env") ?? null, "env", str),
    workspace: opt(
      m,
      "workspace",
      (w): WorkspaceConfig => ({
        repo: str(w.get("repo") ?? null, "workspace.repo"),
        ref: optStr(w, "ref", "workspace"),
      })
    ),
    on: opt(
      m,
      "on",
      (o): PipelineTriggers => ({
        push: opt(o, "push", (p) => ({
          branches: list(p, "branches", "on.push"),
          paths: list(p, "paths", "on.push"),
          paths_ignore: list(p, "paths_ignore", "on.push"),
        })),
        pull_request: opt(o, "pull_request", (p) => ({
          branches: list(p, "branches", "on.pull_request"),
          types: list(p, "types", "on.pull_request"),
          paths: list(p, "paths", "on.pull_request"),
          paths_ignore: list(p, "paths_ignore", "on.pull_request"),
        })),
        interval_minutes: u32(o, "interval_minutes", "on"),
        cron: optStr(o, "cron", "on"),
      })
    ),
    steps: steps(m.get("steps") ?? null),
    timeout_minutes: u32(m, "timeout_minutes", "pipeline"),
    concurrency: opt(
      m,
      "concurrency",
      (c): ConcurrencyConfig => ({
        group: optStr(c, "group", "concurrency"),
        cancel_in_progress: bool(c, "cancel_in_progress", "concurrency"),
      })
    ),
  }
}

/** The authoring path: an unknown key is an error, because ignoring it runs another pipeline. */
export function parsePipelineYaml(text: string): PipelineDefinition {
  const root = parseYaml(text)
  const bad = auditPipelineKeys(root)
  if (bad) throw new DefinitionError(bad)
  return decodeDefinition(root)
}
export const parsePipelineYamlLenient = (text: string): PipelineDefinition =>
  decodeDefinition(parseYaml(text))

/** Audit a JSON submission, including the legacy `{"yaml": "…"}` shape; message or `null`. */
export function auditDefinition(v: unknown): string | null {
  const y = v !== null && typeof v === "object" ? (v as { yaml?: unknown }).yaml : undefined
  if (typeof y !== "string") return auditPipelineKeys(fromJson(v))
  try {
    parsePipelineYaml(y)
    return null
  } catch (e) {
    if (e instanceof DefinitionError) return e.message
    throw e
  }
}
