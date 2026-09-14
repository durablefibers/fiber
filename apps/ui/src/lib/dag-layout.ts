import { type Edge, MarkerType, type Node } from "@xyflow/react"
import type { StepNodeData } from "@/components/step-node"
import type { PipelineDefinition, StepDefinition } from "@/lib/api"

export const COL_WIDTH = 268
// A node measures ~124px, and ~140px once it carries a step id, matrix binding or
// condition row. One pitch for both keeps columns aligned without measuring every node.
export const ROW_HEIGHT = 176
export const ORIGIN_X = 56
export const ORIGIN_Y = 48

export const edgeDefaults = {
  type: "smoothstep" as const,
  animated: false,
  style: { stroke: "var(--canvas-edge)", strokeWidth: 1.75 },
  markerEnd: {
    type: MarkerType.ArrowClosed,
    width: 16,
    height: 16,
    color: "var(--canvas-edge-head)",
  },
}

/** Levels by longest path from a root, so an edge always points rightwards. */
export function levelsOf(steps: StepDefinition[]): Map<string, number> {
  const levels = new Map<string, number>()
  const byId = new Map(steps.map((s) => [s.id, s]))
  const visit = (id: string, seen: Set<string>): number => {
    const known = levels.get(id)
    if (known !== undefined) return known
    // A cycle cannot be laid out in layers; pin the revisited node to 0 and move on
    // rather than recursing forever. The server rejects cyclic definitions anyway.
    if (seen.has(id)) return 0
    seen.add(id)
    const step = byId.get(id)
    const needs = (step?.needs ?? []).filter((n) => byId.has(n))
    const lvl = needs.length
      ? Math.max(...needs.map((n) => visit(n, seen) + 1))
      : 0
    seen.delete(id)
    levels.set(id, lvl)
    return lvl
  }
  for (const s of steps) visit(s.id, new Set())
  return levels
}

/**
 * Order each level so edges cross as little as possible: repeatedly place a node at
 * the average row of its neighbours in the adjacent level (the barycentre heuristic),
 * sweeping forwards then backwards. Two sweeps untangles the fan-in / fan-out shapes
 * a CI DAG actually produces without the cost of a full Sugiyama pass.
 */
export function orderLevels(
  steps: StepDefinition[],
  levels: Map<string, number>
): string[][] {
  const byId = new Map(steps.map((s) => [s.id, s]))
  const maxLevel = Math.max(0, ...levels.values())
  const columns: string[][] = Array.from({ length: maxLevel + 1 }, () => [])
  for (const s of steps) columns[levels.get(s.id) ?? 0].push(s.id)

  const children = new Map<string, string[]>()
  for (const s of steps) {
    for (const n of s.needs ?? []) {
      if (!byId.has(n)) continue
      const list = children.get(n) ?? []
      list.push(s.id)
      children.set(n, list)
    }
  }

  const rowOf = (col: string[]) => new Map(col.map((id, i) => [id, i]))
  const sortBy = (col: string[], key: Map<string, number>) =>
    [...col].sort((a, b) => {
      const ka = key.get(a)
      const kb = key.get(b)
      if (ka === undefined && kb === undefined) return 0
      if (ka === undefined) return 1
      if (kb === undefined) return -1
      return ka - kb
    })

  for (let pass = 0; pass < 2; pass++) {
    for (let l = 1; l < columns.length; l++) {
      const prev = rowOf(columns[l - 1])
      const key = new Map<string, number>()
      for (const id of columns[l]) {
        const parents = (byId.get(id)?.needs ?? [])
          .map((n) => prev.get(n))
          .filter((v): v is number => v !== undefined)
        if (parents.length) {
          key.set(id, parents.reduce((a, b) => a + b, 0) / parents.length)
        }
      }
      columns[l] = sortBy(columns[l], key)
    }
    for (let l = columns.length - 2; l >= 0; l--) {
      const next = rowOf(columns[l + 1])
      const key = new Map<string, number>()
      for (const id of columns[l]) {
        const kids = (children.get(id) ?? [])
          .map((c) => next.get(c))
          .filter((v): v is number => v !== undefined)
        if (kids.length) {
          key.set(id, kids.reduce((a, b) => a + b, 0) / kids.length)
        }
      }
      columns[l] = sortBy(columns[l], key)
    }
  }
  return columns
}

export function nodeData(s: StepDefinition, status?: string): StepNodeData {
  const axes = s.matrix ? Object.entries(s.matrix) : []
  const cells = axes.reduce(
    (n, [, v]) => n * Math.max(v.length, 1),
    axes.length ? 1 : 0
  )
  // A compiled matrix cell carries one value per axis; an authored matrix carries the
  // whole axis. The first is a binding to show, the second a fan-out to advertise.
  const bound = axes.length > 0 && axes.every(([, v]) => v.length === 1)
  return {
    matrixBinding: bound
      ? axes.map(([k, v]) => `${k}: ${v[0]}`).join(", ")
      : undefined,
    label: s.name || s.id,
    stepId: s.id,
    status,
    labels: s.labels,
    retries: s.retries,
    artifacts: s.artifacts,
    needsCount: s.needs?.length ?? 0,
    image: s.image,
    condition: s.if,
    continueOnError: s.continue_on_error,
    matrixCells: !bound && cells > 1 ? cells : undefined,
    matrixAxes: !bound && cells > 1 ? Object.keys(s.matrix ?? {}) : undefined,
    ariaLabel: nodeAriaLabel(s, status),
    runPreview: s.run
      ?.split("\n")
      .map((l) => l.trim())
      .find(Boolean)
      ?.slice(0, 48),
  }
}

/**
 * What a screen reader hears on a node. React Flow makes every node a tab stop with
 * `role="group"`, so without this the whole graph announces as "group, node" repeated
 * once per step and a keyboard user learns nothing from walking it.
 */
export function nodeAriaLabel(s: StepDefinition, status?: string): string {
  const parts = [s.name || s.id]
  parts.push(status ? `status ${status}` : "not started")
  const needs = (s.needs ?? []).length
  if (needs > 0)
    parts.push(needs === 1 ? "needs 1 step" : `needs ${needs} steps`)
  return parts.join(", ")
}

/**
 * Statuses arrive on every poll, and rebuilding each node's `data` would hand React
 * Flow a new object identity for every step every few seconds — which defeats the
 * `memo` on StepNode and re-renders the whole graph to change nothing. Compare first
 * and keep the object that is already there.
 */
export function sameNodeData(a: StepNodeData, b: StepNodeData): boolean {
  const sameList = (x?: string[], y?: string[]) =>
    x === y ||
    (x?.length === y?.length && (x ?? []).every((v, i) => v === y?.[i]))
  return (
    a.label === b.label &&
    a.stepId === b.stepId &&
    a.status === b.status &&
    a.retries === b.retries &&
    a.needsCount === b.needsCount &&
    a.runPreview === b.runPreview &&
    a.image === b.image &&
    a.condition === b.condition &&
    a.continueOnError === b.continueOnError &&
    a.matrixCells === b.matrixCells &&
    a.matrixBinding === b.matrixBinding &&
    a.ariaLabel === b.ariaLabel &&
    sameList(a.labels, b.labels) &&
    sameList(a.artifacts, b.artifacts) &&
    sameList(a.matrixAxes, b.matrixAxes)
  )
}

export function layout(
  steps: StepDefinition[],
  statuses?: Record<string, string>,
  selectedId?: string | null
): { nodes: Node<StepNodeData>[]; edges: Edge[] } {
  const levels = levelsOf(steps)
  const columns = orderLevels(steps, levels)
  const byId = new Map(steps.map((s) => [s.id, s]))

  // Centre every column against the tallest one so a fan-out reads as a fan, not
  // as a ladder hanging off the top edge.
  const tallest = Math.max(1, ...columns.map((c) => c.length))
  const positions = new Map<string, { x: number; y: number }>()
  columns.forEach((col, lvl) => {
    const offset = ((tallest - col.length) * ROW_HEIGHT) / 2
    col.forEach((id, row) => {
      positions.set(id, {
        x: ORIGIN_X + lvl * COL_WIDTH,
        y: ORIGIN_Y + offset + row * ROW_HEIGHT,
      })
    })
  })

  const nodes: Node<StepNodeData>[] = steps.map((s) => ({
    id: s.id,
    type: "step",
    position: positions.get(s.id) ?? { x: ORIGIN_X, y: ORIGIN_Y },
    selected: selectedId === s.id,
    ariaLabel: nodeAriaLabel(s, statuses?.[s.id]),
    data: nodeData(s, statuses?.[s.id]),
  }))

  const edges: Edge[] = []
  for (const s of steps) {
    for (const n of s.needs ?? []) {
      if (!byId.has(n)) continue
      edges.push({
        id: `${n}-${s.id}`,
        source: n,
        target: s.id,
        ...edgeDefaults,
      })
    }
  }
  return { nodes, edges }
}

/**
 * Would adding `source → target` close a loop? React Flow asks before it will show a
 * drop target, so a cycle is refused while dragging rather than drawn and then
 * rejected by the server on save.
 */
export function wouldCycle(
  edges: Pick<Edge, "source" | "target">[],
  source: string,
  target: string
): boolean {
  if (source === target) return true
  const out = new Map<string, string[]>()
  for (const e of edges) {
    const list = out.get(e.source) ?? []
    list.push(e.target)
    out.set(e.source, list)
  }
  // Walk forwards from the proposed target: reaching the source means the new edge
  // would join the end of a path back to its own beginning.
  const stack = [target]
  const seen = new Set<string>()
  while (stack.length) {
    const id = stack.pop()!
    if (id === source) return true
    if (seen.has(id)) continue
    seen.add(id)
    stack.push(...(out.get(id) ?? []))
  }
  return false
}

/**
 * Recover a compiled matrix cell's bindings from the step name the DAG compiler wrote.
 *
 * `fiber-core`'s `expand_all` renders a cell's bindings as `k1: v1, k2: v2` with the
 * axes in `BTreeMap` order, so the separators are found by looking for the *next* axis
 * name rather than by splitting on every comma — a value may legitimately contain one.
 * Returns an empty record if the text does not have the shape we expect, which leaves
 * the caller showing the compiled name unchanged rather than a wrong binding.
 */
export function parseMatrixBindings(
  pretty: string,
  axes: string[]
): Record<string, string[]> {
  const sorted = [...axes].sort()
  const out: Record<string, string[]> = {}
  let rest = pretty
  for (let i = 0; i < sorted.length; i++) {
    const key = sorted[i]
    const prefix = `${key}: `
    if (!rest.startsWith(prefix)) return {}
    rest = rest.slice(prefix.length)
    const next = sorted[i + 1]
    if (next === undefined) {
      out[key] = [rest]
      break
    }
    const marker = `, ${next}: `
    const at = rest.indexOf(marker)
    if (at < 0) return {}
    out[key] = [rest.slice(0, at)]
    rest = rest.slice(at + 2)
  }
  return out
}

/** Everything that changes where a node goes. Data-only edits must not re-lay-out. */
export function topologyKey(steps: StepDefinition[]): string {
  return steps.map((s) => `${s.id}>${(s.needs ?? []).join(",")}`).join("|")
}

export function toDefinition(
  nodes: Node[],
  edges: Edge[],
  prev: PipelineDefinition
): PipelineDefinition {
  const prevById = new Map(prev.steps.map((s) => [s.id, s]))
  const needsMap = new Map<string, string[]>()
  for (const e of edges) {
    const list = needsMap.get(e.target) ?? []
    list.push(e.source)
    needsMap.set(e.target, list)
  }
  const live = new Set(nodes.map((n) => n.id))
  return {
    // Spread the previous definition so pipeline-level fields the canvas does not
    // model (env, timeout_minutes, on, workspace) survive an edge edit.
    ...prev,
    steps: nodes.map((n) => {
      const prevStep = prevById.get(n.id)
      const data = n.data as StepNodeData
      const base: StepDefinition = prevStep ?? {
        id: n.id,
        name: data.label || n.id,
        needs: [],
        run: 'echo "todo"',
        labels: data.labels ?? ["os=linux"],
        retries: data.retries ?? 0,
        artifacts: data.artifacts ?? [],
      }
      // Keep every field the step already had; only `needs` is canvas-owned.
      return {
        ...base,
        needs: (needsMap.get(n.id) ?? []).filter((id) => live.has(id)),
      }
    }),
  }
}
