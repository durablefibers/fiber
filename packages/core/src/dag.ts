import {
  artifactPathOk,
  asciiUpper,
  byteLength,
  type CompiledDag,
  type CompiledStep,
  compareCodePoints,
  gitRefOk,
  imageReferenceOk,
  type PipelineDefinition,
  repoUrlOk,
  type StepDefinition,
  shellOk,
  trim,
  workingDirectoryOk,
} from "@fiber/proto"
import { validateIf, validateKey } from "./step-if.ts"

const MAX_MATRIX_CELLS = 64
export const DEFAULT_MAX_STEPS = 500
/** The compiled form is stored per run, so its expanded size is bounded, not just its count. */
export const MAX_EXPANDED_BYTES = 8 * 1024 * 1024
/** Bounds for values that become timers and counters. */
export const MAX_TIMEOUT_MINUTES = 7 * 24 * 60
export const MAX_RETRIES = 20

export class DagError extends Error {}

/** Parse a configured step cap. Unset is the default; anything unusable is an error. */
export function maxStepsFrom(raw: string | null | undefined): number {
  const s = raw ? trim(raw) : ""
  if (s === "") return DEFAULT_MAX_STEPS
  const n = /^\+?[0-9]+$/.test(s) ? Number(s) : 0
  if (n === 0 || !Number.isSafeInteger(n))
    throw new Error(`FIBER_MAX_STEPS=\`${s}\` is not a positive whole number of steps`)
  return n
}

interface Cell {
  id: string
  name: string
  matrix: Record<string, string>
  env: [string, string][]
  step: StepDefinition
}
const envNameOk = (s: string): boolean => /^[A-Za-z_][A-Za-z0-9_]*$/.test(s)
const sanitize = (s: string): string =>
  Array.from(s, (c) => (/[A-Za-z0-9_]/.test(c) ? c : "_")).join("")

function matrixCombos(step: StepDefinition): [string, string][][] {
  const axes = Object.entries(step.matrix ?? {}).sort((a, b) => compareCodePoints(a[0], b[0]))
  const badAxis = axes.find(([k]) => !envNameOk(k))
  if (badAxis)
    throw new DagError(
      `step \`${step.id}\` matrix axis \`${badAxis[0]}\` is not a usable environment variable name (letters, digits and \`_\`, not starting with a digit)`
    )
  for (const [axis, values] of axes)
    if (values.length === 0)
      throw new DagError(`step \`${step.id}\` matrix axis \`${axis}\` is empty`)
  let combos: [string, string][][] = [[]]
  for (const [axis, values] of axes) {
    // Checked before the product is built.
    if (combos.length * values.length > MAX_MATRIX_CELLS)
      throw new DagError(
        `step \`${step.id}\` matrix expands to more than ${MAX_MATRIX_CELLS} cells`
      )
    combos = combos.flatMap((c) => values.map((v): [string, string][] => [...c, [axis, v]]))
  }
  return combos
}

const strings = (v: readonly string[]): number => v.reduce((n, s) => n + byteLength(s) + 1, 0)
const len = (s: string | null): number => (s === null ? 0 : byteLength(s))
/** The text every cell of a step gets its own copy of. */
const stepBytes = (s: StepDefinition): number =>
  len(s.run) +
  len(s.image) +
  len(s.working_directory) +
  len(s.shell) +
  len(s.if) +
  strings(s.labels) +
  strings(s.artifacts) +
  strings(s.needs) +
  (s.secrets ? strings(s.secrets) : 0)

function checkStep(def: PipelineDefinition, step: StepDefinition): void {
  for (const k of [...Object.keys(def.env), ...Object.keys(step.env)]) {
    if (!envNameOk(k))
      throw new DagError(
        `step \`${step.id}\` env name \`${k}\` is not a usable environment variable name`
      )
    if (k.startsWith("FIBER_"))
      throw new DagError(
        `step \`${step.id}\` env name \`${k}\` is reserved: FIBER_* is set by the server`
      )
  }
  if (step.working_directory !== null && !workingDirectoryOk(step.working_directory))
    throw new DagError(
      `step \`${step.id}\` working_directory \`${step.working_directory}\` must stay inside the workspace`
    )
  if (step.shell !== null && !shellOk(step.shell))
    throw new DagError(`step \`${step.id}\` shell \`${step.shell}\` must be a bare program name`)
  const bad = step.artifacts.find((p) => !artifactPathOk(trim(p)))
  if (bad !== undefined)
    throw new DagError(
      `step \`${step.id}\` artifact \`${bad}\` must be a plain relative path inside the workspace (no leading \`/\` or \`./\`, no \`..\`)`
    )
  const image = step.image === null ? "" : trim(step.image)
  if (image !== "" && !imageReferenceOk(image))
    throw new DagError(
      `step \`${step.id}\` image \`${image}\` is not a docker image reference (\`name[:tag][@digest]\`, optionally registry-qualified)`
    )
}

function expandAll(def: PipelineDefinition, max: number): Cell[] {
  const out: Cell[] = []
  let bytes = 0
  for (const step of def.steps) {
    const combos = matrixCombos(step)
    const perCell = stepBytes(step)
    if (out.length + combos.length > max) throw tooManySteps(out.length + combos.length, max)
    checkStep(def, step)
    for (const combo of combos) {
      let { id, name } = step
      // Least specific first; the cell's own bindings win, so logs cannot lie about it.
      const env = new Map<string, string>([...Object.entries(def.env), ...Object.entries(step.env)])
      if (combo.length > 0) {
        id = `${id}__${combo.map(([k, v]) => `${sanitize(k)}_${sanitize(v)}`).join("__")}`
        name = `${name} (${combo.map(([k, v]) => `${k}: ${v}`).join(", ")})`
        for (const [k, v] of combo) {
          env.set(k, v)
          env.set(`MATRIX_${asciiUpper(k)}`, v)
          env.set(`FIBER_MATRIX_${asciiUpper(k)}`, v)
        }
      }
      bytes += byteLength(id) + byteLength(name) + perCell
      for (const [k, v] of env) bytes += byteLength(k) + byteLength(v) + 2
      if (bytes > MAX_EXPANDED_BYTES)
        throw new DagError(
          `pipeline expands to at least ${bytes} bytes of step definitions, more than the limit of ${MAX_EXPANDED_BYTES} — pipeline \`env\` is merged into every expanded step, so a large value is multiplied by the number of steps`
        )
      out.push({ id, name, matrix: Object.fromEntries(combo), env: [...env], step })
    }
  }
  const seen = new Set<string>()
  for (const c of out) {
    if (seen.has(c.id)) throw new DagError(`duplicate step id: ${c.id}`)
    seen.add(c.id)
  }
  return out
}

const tooManySteps = (count: number, max: number): DagError =>
  new DagError(
    `pipeline expands to at least ${count} steps, more than the limit of ${max} (raise FIBER_MAX_STEPS to allow more)`
  )

function checkTimeout(what: string, t: number | null): void {
  if (t === 0) throw new DagError(`\`${what}\`: timeout_minutes must be at least 1`)
  if (t !== null && t > MAX_TIMEOUT_MINUTES)
    throw new DagError(`\`${what}\`: timeout_minutes must be at most ${MAX_TIMEOUT_MINUTES}`)
}

export function compileDefinition(
  def: PipelineDefinition,
  maxSteps: number = DEFAULT_MAX_STEPS
): CompiledDag {
  if (def.steps.length === 0) throw new DagError("pipeline must have at least one step")
  checkTimeout("pipeline", def.timeout_minutes)
  if (def.workspace) {
    if (!repoUrlOk(def.workspace.repo))
      throw new DagError(
        `workspace repo \`${def.workspace.repo}\` must be an http(s), ssh, git, or file URL, an scp-like \`user@host:path\`, or a path`
      )
    if (def.workspace.ref !== null && !gitRefOk(def.workspace.ref))
      throw new DagError(
        `workspace ref \`${def.workspace.ref}\` must be a single branch, tag or ref name (no leading \`-\`, \`..\`, spaces, or \`~ ^ : ?\`)`
      )
  }
  for (const s of def.steps) {
    checkTimeout(`step ${s.id}`, s.timeout_minutes)
    if (s.retries > MAX_RETRIES)
      throw new DagError(`step \`${s.id}\`: retries must be at most ${MAX_RETRIES}`)
  }
  const ids = new Set<string>()
  for (const s of def.steps) {
    if (trim(s.id) === "") throw new DagError("every step needs a non-empty id")
    if (ids.has(s.id)) throw new DagError(`duplicate step id: ${s.id}`)
    ids.add(s.id)
    const bad = validateIf(s.if)
    if (bad) throw new DagError(`step \`${s.id}\` has an unusable condition — ${bad}`)
    if (s.run === null || trim(s.run) === "")
      throw new DagError(`step \`${s.id}\` is missing a run command`)
  }
  for (const s of def.steps)
    for (const dep of s.needs)
      if (!ids.has(dep)) throw new DagError(`unknown dependency \`${dep}\` on step \`${s.id}\``)
  // Every step yields at least one cell, so this is decidable before expanding any.
  if (def.steps.length > maxSteps) throw tooManySteps(def.steps.length, maxSteps)

  const cells = expandAll(def, maxSteps)
  const byBase = new Map<string, string[]>()
  for (const c of cells) {
    const l = byBase.get(c.step.id)
    if (l) l.push(c.id)
    else byBase.set(c.step.id, [c.id])
  }
  // Repeated needs are collapsed before they are rewritten to matrix cells.
  const needs = new Map(
    cells.map((c) => [c.id, [...new Set(c.step.needs)].flatMap((d) => byBase.get(d) ?? [])])
  )
  const levelOf = computeLevels(needs)

  const levels: string[][] = []
  const steps = cells.map((c): CompiledStep => {
    const bad = validateKey(c.step.if, c.env)
    if (bad) throw new DagError(`step \`${c.id}\` has an unusable condition — ${bad}`)
    const level = levelOf.get(c.id) as number
    while (levels.length <= level) levels.push([])
    ;(levels[level] as string[]).push(c.id)
    return {
      id: c.id,
      name: c.name,
      needs: needs.get(c.id) as string[],
      run: c.step.run ?? "",
      image: (c.step.image && trim(c.step.image)) || null,
      labels: c.step.labels,
      retries: c.step.retries,
      artifacts: c.step.artifacts,
      level,
      matrix: c.matrix,
      env: c.env,
      if: c.step.if,
      timeout_minutes: c.step.timeout_minutes,
      secrets: c.step.secrets,
      working_directory: c.step.working_directory,
      shell: c.step.shell,
      continue_on_error: c.step.continue_on_error,
    }
  })
  return {
    name: def.name,
    workspace: def.workspace && { ...def.workspace, repo: trim(def.workspace.repo) },
    steps,
    levels,
    timeout_minutes: def.timeout_minutes,
  }
}

/** Longest path from a root. Iterative, so a 500-step chain cannot exhaust the stack. */
function computeLevels(needs: Map<string, string[]>): Map<string, number> {
  const waiting = new Map([...needs].map(([id, n]) => [id, n.length]))
  const dependents = new Map<string, string[]>()
  for (const [id, deps] of needs)
    for (const d of deps) {
      const l = dependents.get(d)
      if (l) l.push(id)
      else dependents.set(d, [id])
    }
  const level = new Map<string, number>()
  const ready = [...waiting].filter(([, n]) => n === 0).map(([id]) => id)
  for (const id of ready) level.set(id, 0)
  for (let i = 0; i < ready.length; i++) {
    const id = ready[i] as string
    for (const d of dependents.get(id) ?? []) {
      level.set(d, Math.max(level.get(d) ?? 0, (level.get(id) as number) + 1))
      const left = (waiting.get(d) as number) - 1
      waiting.set(d, left)
      if (left === 0) ready.push(d)
    }
  }
  if (ready.length !== needs.size) throw new DagError("pipeline contains a cycle")
  return level
}
