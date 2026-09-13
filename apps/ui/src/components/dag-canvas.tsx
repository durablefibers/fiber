import {
  addEdge,
  Background,
  BackgroundVariant,
  type Connection,
  Controls,
  type Edge,
  MiniMap,
  type Node,
  Panel,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesState,
  useReactFlow,
} from "@xyflow/react"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import "@xyflow/react/dist/style.css"
import { LayoutGrid, Plus } from "lucide-react"
import { StepNode, type StepNodeData } from "@/components/step-node"
import type { PipelineDefinition } from "@/lib/api"
import { statusColor } from "@/lib/api"
import {
  edgeDefaults,
  layout,
  nodeData,
  ORIGIN_X,
  ORIGIN_Y,
  ROW_HEIGHT,
  toDefinition,
  topologyKey,
  wouldCycle,
} from "@/lib/dag-layout"

const nodeTypes = { step: StepNode }

const canvasSurfaceClass =
  "relative h-full min-h-[320px] w-full overflow-hidden rounded-2xl border border-white/10 bg-[radial-gradient(ellipse_at_top,oklch(0.22_0.028_250),oklch(0.125_0.012_260)_60%)]"

export function DagCanvas(props: {
  definition: PipelineDefinition
  statuses?: Record<string, string>
  editable?: boolean
  selectedStepId?: string | null
  onChange?: (def: PipelineDefinition) => void
  onSelectStep?: (stepId: string | null) => void
}) {
  const [mounted, setMounted] = useState(false)
  useEffect(() => setMounted(true), [])

  if (!mounted) {
    return <div className={canvasSurfaceClass} />
  }

  return (
    <ReactFlowProvider>
      <DagCanvasInner {...props} />
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
  const { fitView } = useReactFlow()
  // biome-ignore lint/correctness/useExhaustiveDependencies: seed once; later updates via effects
  const initial = useMemo(
    () => layout(definition.steps, statuses, selectedStepId),
    []
  )
  const [nodes, setNodes, onNodesChange] = useNodesState(initial.nodes)
  const [edges, setEdges, onEdgesChange] = useEdgesState(initial.edges)
  const topology = topologyKey(definition.steps)
  const definitionRef = useRef(definition)
  definitionRef.current = definition
  const fittedRef = useRef(false)

  // Positions are recomputed only when the shape of the graph changes, and a node the
  // viewer has already dragged keeps where they put it. Re-laying out on every status
  // tick would yank the canvas around under them during a live run.
  // biome-ignore lint/correctness/useExhaustiveDependencies: keyed on topology; the definition is read through a ref on purpose
  useEffect(() => {
    const next = layout(definitionRef.current.steps)
    setNodes((prev) => {
      const placed = new Map(prev.map((n) => [n.id, n.position]))
      return next.nodes.map((n) => {
        const kept = placed.get(n.id)
        return kept ? { ...n, position: kept } : n
      })
    })
    setEdges(next.edges)
    // The `fitView` prop fits the seed layout, which is laid out before the columns
    // are centred. Fit once more when real nodes first arrive, then leave the
    // viewport alone so panning and zooming stick.
    if (!fittedRef.current && next.nodes.length > 0) {
      fittedRef.current = true
      window.setTimeout(() => void fitView({ padding: 0.24, maxZoom: 1.05 }), 0)
    }
  }, [topology, setNodes, setEdges, fitView])

  // Labels, statuses and selection are data-only: patch the nodes in place.
  useEffect(() => {
    const byId = new Map(definition.steps.map((s) => [s.id, s]))
    setNodes((prev) =>
      prev.map((n) => {
        const step = byId.get(n.id)
        if (!step) return n
        return {
          ...n,
          selected: selectedStepId === n.id,
          data: nodeData(step, statuses?.[n.id]),
        }
      })
    )
  }, [definition.steps, statuses, selectedStepId, setNodes])

  const isValidConnection = useCallback(
    (c: Connection | Edge) =>
      Boolean(c.source && c.target) && !wouldCycle(edges, c.source, c.target),
    [edges]
  )

  const onConnect = useCallback(
    (connection: Connection) => {
      if (!editable) return
      if (!isValidConnection(connection)) return
      setEdges((eds) => {
        const next = addEdge({ ...connection, ...edgeDefaults }, eds)
        onChange?.(toDefinition(nodes, next, definitionRef.current))
        return next
      })
    },
    [editable, setEdges, onChange, nodes, isValidConnection]
  )

  const onEdgesDelete = useCallback(
    (deleted: Edge[]) => {
      if (!editable || !onChange) return
      const del = new Set(deleted.map((e) => e.id))
      setEdges((eds) => {
        const next = eds.filter((e) => !del.has(e.id))
        onChange(toDefinition(nodes, next, definitionRef.current))
        return next
      })
    },
    [editable, onChange, nodes, setEdges]
  )

  // Deleting a node on the canvas has to reach the definition too, or the graph and
  // the saved pipeline silently disagree.
  const onNodesDelete = useCallback(
    (deleted: Node[]) => {
      if (!editable || !onChange) return
      const gone = new Set(deleted.map((n) => n.id))
      const keptNodes = nodes.filter((n) => !gone.has(n.id))
      const keptEdges = edges.filter(
        (e) => !gone.has(e.source) && !gone.has(e.target)
      )
      onChange(toDefinition(keptNodes, keptEdges, definitionRef.current))
      if (selectedStepId && gone.has(selectedStepId)) onSelectStep?.(null)
    },
    [editable, onChange, nodes, edges, selectedStepId, onSelectStep]
  )

  const addStep = () => {
    const existing = new Set(nodes.map((n) => n.id))
    let id = `step_${Math.random().toString(36).slice(2, 7)}`
    while (existing.has(id))
      id = `step_${Math.random().toString(36).slice(2, 7)}`
    const node: Node<StepNodeData> = {
      id,
      type: "step",
      position: { x: ORIGIN_X, y: ORIGIN_Y + nodes.length * ROW_HEIGHT },
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
    onChange?.(toDefinition(nextNodes, edges, definitionRef.current))
    onSelectStep?.(id)
  }

  /** Throw away hand-placed positions and re-run the layered layout. */
  const relayout = () => {
    const next = layout(definition.steps, statuses, selectedStepId)
    setNodes(next.nodes)
    setEdges(next.edges)
    window.setTimeout(() => void fitView({ padding: 0.28, duration: 220 }), 0)
  }

  return (
    <div className={canvasSurfaceClass}>
      <div className="absolute top-3 right-3 z-10 flex gap-2">
        <button
          type="button"
          onClick={relayout}
          title="Tidy layout"
          className="inline-flex items-center gap-1.5 rounded-lg bg-white/8 px-2.5 py-1.5 font-medium text-white/80 text-xs backdrop-blur transition hover:bg-white/16 hover:text-white"
        >
          <LayoutGrid className="size-3.5" />
          Tidy
        </button>
        {editable ? (
          <button
            type="button"
            onClick={addStep}
            className="inline-flex items-center gap-1.5 rounded-lg bg-white/10 px-3 py-1.5 font-medium text-white text-xs backdrop-blur transition hover:bg-white/16"
          >
            <Plus className="size-3.5" />
            Add step
          </button>
        ) : null}
      </div>
      <ReactFlow
        nodes={nodes}
        edges={edges}
        // Always wired, even read-only: these handlers are how React Flow reports
        // measured dimensions and selection back into controlled state, and the
        // minimap draws nothing for nodes whose size never made it back. Dragging
        // is gated by `nodesDraggable`, not by withholding the handler.
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onConnect={onConnect}
        onEdgesDelete={editable ? onEdgesDelete : undefined}
        onNodesDelete={editable ? onNodesDelete : undefined}
        isValidConnection={isValidConnection}
        onNodeClick={(_, node) => onSelectStep?.(node.id)}
        onPaneClick={() => onSelectStep?.(null)}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.28, maxZoom: 1.05, minZoom: 0.45 }}
        nodesDraggable={editable}
        nodesConnectable={editable}
        nodesFocusable
        elementsSelectable
        deleteKeyCode={editable ? ["Backspace", "Delete"] : null}
        defaultEdgeOptions={edgeDefaults}
        className="!bg-transparent h-full w-full"
        minZoom={0.35}
        maxZoom={1.5}
        proOptions={{ hideAttribution: false }}
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
          className="!hidden !rounded-lg !border-white/10 !bg-[oklch(0.14_0.012_260)] sm:!block"
          style={{ width: 150, height: 104 }}
          nodeColor={(n) => {
            const data = n.data as StepNodeData | undefined
            if (n.selected) return "#38bdf8"
            return data?.status
              ? statusColor(data.status)
              : "rgba(125,211,252,0.6)"
          }}
          nodeStrokeWidth={2}
          nodeBorderRadius={3}
          maskColor="rgba(0,0,0,0.62)"
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
