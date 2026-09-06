import {
  addEdge,
  Background,
  BackgroundVariant,
  type Connection,
  Controls,
  type Edge,
  MarkerType,
  MiniMap,
  type Node,
  Panel,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesState,
} from "@xyflow/react"
import { useCallback, useEffect, useMemo, useState } from "react"
import "@xyflow/react/dist/style.css"
import { Plus } from "lucide-react"
import { StepNode, type StepNodeData } from "@/components/step-node"
import type { PipelineDefinition, StepDefinition } from "@/lib/api"

const nodeTypes = { step: StepNode }

const canvasSurfaceClass =
  "relative h-full min-h-[480px] w-full overflow-hidden rounded-2xl border border-white/10 bg-[radial-gradient(ellipse_at_top,oklch(0.22_0.028_250),oklch(0.125_0.012_260)_60%)]"

const edgeDefaults = {
  type: "smoothstep" as const,
  animated: false,
  style: { stroke: "rgba(125,211,252,0.55)", strokeWidth: 1.75 },
  markerEnd: {
    type: MarkerType.ArrowClosed,
    width: 16,
    height: 16,
    color: "rgba(125,211,252,0.7)",
  },
}

function layout(
  steps: StepDefinition[],
  statuses?: Record<string, string>,
  selectedId?: string | null
): { nodes: Node<StepNodeData>[]; edges: Edge[] } {
  const levels = new Map<string, number>()
  const byId = new Map(steps.map((s) => [s.id, s]))
  function levelOf(id: string, seen = new Set<string>()): number {
    if (levels.has(id)) return levels.get(id)!
    if (seen.has(id)) return 0
    seen.add(id)
    const step = byId.get(id)
    const lvl = step?.needs?.length
      ? Math.max(...step.needs.map((n) => levelOf(n, seen) + 1))
      : 0
    levels.set(id, lvl)
    return lvl
  }
  for (const s of steps) {
    levelOf(s.id)
  }

  const columns = new Map<number, number>()
  const nodes: Node<StepNodeData>[] = steps.map((s) => {
    const lvl = levels.get(s.id) ?? 0
    const row = columns.get(lvl) ?? 0
    columns.set(lvl, row + 1)
    return {
      id: s.id,
      type: "step",
      position: { x: 56 + lvl * 268, y: 56 + row * 118 },
      selected: selectedId === s.id,
      data: {
        label: s.name || s.id,
        stepId: s.id,
        status: statuses?.[s.id],
        labels: s.labels,
        retries: s.retries,
        artifacts: s.artifacts,
        needsCount: s.needs?.length ?? 0,
        runPreview: s.run
          ?.split("\n")
          .map((l) => l.trim())
          .find(Boolean)
          ?.slice(0, 48),
      },
    }
  })

  const edges: Edge[] = []
  for (const s of steps) {
    for (const n of s.needs ?? []) {
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

function toDefinition(
  name: string,
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
  return {
    name,
    on: prev.on,
    workspace: prev.workspace,
    steps: nodes.map((n) => {
      const prevStep = prevById.get(n.id)
      const data = n.data as StepNodeData
      return {
        id: n.id,
        name: data.label || n.id,
        needs: needsMap.get(n.id) ?? [],
        run: prevStep?.run ?? 'echo "todo"',
        image: prevStep?.image,
        labels: prevStep?.labels ?? data.labels ?? ["os=linux"],
        retries: prevStep?.retries ?? data.retries ?? 0,
        artifacts: prevStep?.artifacts ?? data.artifacts ?? [],
        if: prevStep?.if,
        matrix: prevStep?.matrix,
      }
    }),
  }
}

export function DagCanvas({
  definition,
  statuses,
  editable = true,
  selectedStepId,
  onChange,
  onSelectStep,
}: {
  definition: PipelineDefinition
  statuses?: Record<string, string>
  editable?: boolean
  selectedStepId?: string | null
  onChange?: (def: PipelineDefinition) => void
  onSelectStep?: (stepId: string | null) => void
}) {
  const [mounted, setMounted] = useState(false)
  useEffect(() => {
    setMounted(true)
    const original = console.warn
    console.warn = (...args: unknown[]) => {
      const first = args[0]
      if (
        typeof first === "string" &&
        (first.includes("[React Flow]") || first.includes("React Flow"))
      ) {
        return
      }
      original.apply(console, args as Parameters<typeof console.warn>)
    }
    return () => {
      console.warn = original
    }
  }, [])

  if (!mounted) {
    return <div className={canvasSurfaceClass} />
  }

  return (
    <ReactFlowProvider>
      <DagCanvasInner
        definition={definition}
        statuses={statuses}
        editable={editable}
        selectedStepId={selectedStepId}
        onChange={onChange}
        onSelectStep={onSelectStep}
      />
    </ReactFlowProvider>
  )
}

function DagCanvasInner({
  definition,
  statuses,
  editable = true,
  selectedStepId,
  onChange,
  onSelectStep,
}: {
  definition: PipelineDefinition
  statuses?: Record<string, string>
  editable?: boolean
  selectedStepId?: string | null
  onChange?: (def: PipelineDefinition) => void
  onSelectStep?: (stepId: string | null) => void
}) {
  // biome-ignore lint/correctness/useExhaustiveDependencies: seed layout once; later updates via effect
  const initial = useMemo(
    () => layout(definition.steps, statuses, selectedStepId),
    []
  )
  const [nodes, setNodes, onNodesChange] = useNodesState(initial.nodes)
  const [edges, setEdges, onEdgesChange] = useEdgesState(initial.edges)

  useEffect(() => {
    const next = layout(definition.steps, statuses, selectedStepId)
    setNodes(next.nodes)
    setEdges(next.edges)
  }, [definition, statuses, selectedStepId, setNodes, setEdges])

  const onConnect = useCallback(
    (connection: Connection) => {
      if (!editable) return
      setEdges((eds) => {
        const next = addEdge({ ...connection, ...edgeDefaults }, eds)
        onChange?.(toDefinition(definition.name, nodes, next, definition))
        return next
      })
    },
    [editable, setEdges, onChange, definition, nodes]
  )

  const onEdgesDelete = useCallback(
    (deleted: Edge[]) => {
      if (!editable || !onChange) return
      const del = new Set(deleted.map((e) => e.id))
      setEdges((eds) => {
        const next = eds.filter((e) => !del.has(e.id))
        onChange(toDefinition(definition.name, nodes, next, definition))
        return next
      })
    },
    [editable, onChange, definition, nodes, setEdges]
  )

  const addStep = () => {
    const id = `step_${Math.random().toString(36).slice(2, 7)}`
    const node: Node<StepNodeData> = {
      id,
      type: "step",
      position: { x: 80, y: 48 + nodes.length * 128 },
      data: {
        label: id,
        stepId: id,
        labels: ["os=linux"],
        retries: 0,
        artifacts: [],
      },
    }
    const nextNodes = [...nodes, node]
    setNodes(nextNodes)
    onChange?.(toDefinition(definition.name, nextNodes, edges, definition))
    onSelectStep?.(id)
  }

  return (
    <div className={canvasSurfaceClass}>
      {editable ? (
        <div className="absolute top-3 right-3 z-10 flex gap-2">
          <button
            type="button"
            onClick={addStep}
            className="inline-flex items-center gap-1.5 rounded-lg bg-white/10 px-3 py-1.5 font-medium text-white text-xs backdrop-blur transition hover:bg-white/16"
          >
            <Plus className="size-3.5" />
            Add step
          </button>
        </div>
      ) : null}
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={editable ? onNodesChange : undefined}
        onEdgesChange={editable ? onEdgesChange : undefined}
        onConnect={onConnect}
        onEdgesDelete={editable ? onEdgesDelete : undefined}
        onNodeClick={(_, node) => onSelectStep?.(node.id)}
        onPaneClick={() => onSelectStep?.(null)}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.28, maxZoom: 1.05, minZoom: 0.45 }}
        nodesDraggable={editable}
        nodesConnectable={editable}
        elementsSelectable
        defaultEdgeOptions={edgeDefaults}
        className="!bg-transparent h-full w-full"
        minZoom={0.35}
        maxZoom={1.5}
      >
        <Background
          id="dots"
          variant={BackgroundVariant.Dots}
          color="rgba(255,255,255,0.18)"
          gap={16}
          size={1.5}
        />
        <Controls className="!border-white/10 !bg-black/45 !shadow-none [&>button]:!border-white/10 [&>button]:!bg-transparent [&>button]:!text-white/80" />
        <MiniMap
          className="!border-white/10 !bg-black/35"
          nodeColor={(n) => (n.selected ? "#38bdf8" : "rgba(125,211,252,0.55)")}
          maskColor="rgba(0,0,0,0.55)"
          pannable
          zoomable
        />
        {definition.steps.length === 0 ? (
          <Panel
            position="top-center"
            className="mt-16 rounded-lg bg-black/40 px-4 py-3 text-center text-white/60 text-xs backdrop-blur"
          >
            Empty pipeline — add a step or import fiber.yml
          </Panel>
        ) : null}
      </ReactFlow>
    </div>
  )
}
