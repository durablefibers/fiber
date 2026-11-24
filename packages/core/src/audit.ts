import { asciiLower, trim } from "@fiber/proto"
import type { Value } from "./yaml.ts"

/** Field names each level accepts, in the order the docs list them. */
export const KEYS = {
  pipeline: ["name", "env", "workspace", "on", "steps", "timeout_minutes", "concurrency"],
  step: [
    "id",
    "name",
    "needs",
    "run",
    "image",
    "labels",
    "retries",
    "env",
    "working_directory",
    "shell",
    "continue_on_error",
    "artifacts",
    "matrix",
    "if",
    "timeout_minutes",
    "secrets",
  ],
  workspace: ["repo", "ref"],
  on: ["push", "pull_request", "interval_minutes", "cron"],
  push: ["branches", "paths", "paths_ignore"],
  pull_request: ["branches", "types", "paths", "paths_ignore"],
  concurrency: ["group", "cancel_in_progress"],
} as const

function editDistance(a: string[], b: string[]): number {
  let prev = b.map((_, i) => i)
  prev.push(b.length)
  for (let i = 0; i < a.length; i++) {
    const cur = [i + 1]
    for (let j = 0; j < b.length; j++)
      cur.push(
        Math.min(
          (prev[j] as number) + (a[i] === b[j] ? 0 : 1),
          (prev[j + 1] as number) + 1,
          (cur[j] as number) + 1
        )
      )
    prev = cur
  }
  return prev[b.length] as number
}

/** The valid field a typo most likely meant, or `null` when nothing is close. */
export function nearest(key: string, allowed: readonly string[]): string | null {
  const norm = (s: string): string => asciiLower(s).replaceAll("-", "_")
  const target = norm(key)
  const exact = allowed.find((a) => norm(a) === target)
  if (exact !== undefined) return exact
  const chars = Array.from(target)
  const budget = Math.max(1, Math.floor(chars.length / 3))
  let best: { d: number; a: string } | null = null
  for (const a of allowed) {
    const d = editDistance(chars, Array.from(norm(a)))
    if (d > budget) continue
    if (!best || d < best.d || (d === best.d && a.length < best.a.length)) best = { d, a }
  }
  return best ? best.a : null
}

function auditKeys(
  what: string,
  map: Map<Value, Value>,
  allowed: readonly string[],
  extensions: boolean
): string | null {
  for (const key of map.keys()) {
    if (typeof key !== "string") return `${what}: keys must be strings`
    if (allowed.includes(key)) continue
    if (key === "<<")
      return `${what}: YAML merge keys (\`<<\`) are not supported — the merged fields would be dropped; write them out on the step`
    // Anchor holders, top level only: elsewhere a `_` would hide a typo.
    if (extensions && (key.startsWith("x-") || key.startsWith("_"))) continue
    const hint = nearest(key, allowed)
    return hint === null
      ? `${what}: unknown field \`${key}\`; valid fields are ${allowed.join(", ")}`
      : `${what}: unknown field \`${key}\` — did you mean \`${hint}\`?`
  }
  return null
}
const get = (v: Value | undefined, k: string): Value | undefined =>
  v instanceof Map ? v.get(k) : undefined
const auditMap = (v: Value | undefined, what: string, allowed: readonly string[]): string | null =>
  v instanceof Map ? auditKeys(what, v, allowed, false) : null

/** The first key the schema does not read, as a message, or `null`. */
export function auditPipelineKeys(root: Value): string | null {
  if (!(root instanceof Map)) return null
  const on = root.get("on")
  const bad =
    auditKeys("pipeline", root, KEYS.pipeline, true) ??
    auditMap(root.get("workspace"), "workspace", KEYS.workspace) ??
    auditMap(on, "on", KEYS.on) ??
    auditMap(get(on, "push"), "on.push", KEYS.push) ??
    auditMap(get(on, "pull_request"), "on.pull_request", KEYS.pull_request) ??
    auditMap(root.get("concurrency"), "concurrency", KEYS.concurrency)
  if (bad) return bad
  const steps = root.get("steps")
  if (Array.isArray(steps)) {
    for (const [i, s] of steps.entries()) {
      const id = get(s, "id")
      const b = auditMap(s, typeof id === "string" ? `step \`${id}\`` : `step #${i + 1}`, KEYS.step)
      if (b) return b
    }
  } else if (steps instanceof Map) {
    for (const [id, s] of steps) {
      if (typeof id !== "string") return "steps: every step id must be a string"
      if (trim(id) === "") return "steps: a step id cannot be empty"
      if (get(s, "id") !== undefined)
        return `step \`${id}\`: remove \`id:\` — in the map form of \`steps:\` the key is the id`
      const b = auditMap(s, `step \`${id}\``, KEYS.step)
      if (b) return b
    }
  }
  return null
}
