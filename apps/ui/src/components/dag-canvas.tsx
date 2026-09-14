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
import { LayoutGrid, Maximize2, Minimize2, Plus } from "lucide-react"
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
  sameNodeData,
  toDefinition,
  topologyKey,
  wouldCycle,
} from "@/lib/dag-layout"
import { motionDuration } from "@/lib/motion"
import { cn } from "@/lib/utils"

const nodeTypes = { step: StepNode }

const canvasSurfaceClass =
  "relative h-full min-h-[320px] w-full overflow-hidden rounded-2xl border border-canvas-border bg-[radial-gradient(ellipse_at_top,var(--canvas-glow),var(--canvas)_60%)]"

const toolbarButtonClass =
  "inline-flex items-center gap-1.5 rounded-lg bg-canvas-raise px-2.5 py-1.5 font-medium text-canvas-fg-muted text-xs backdrop-blur transition hover:bg-canvas-raise-hover hover:text-canvas-fg focus-visible:outline-2 focus-visible:outline-canvas-accent focus-visible:outline-offset-2"

export function DagCanvas(props: {
  definition: PipelineDefinition
  statuses?: Record<string, string>
  editable?: boolean
  selectedStepId?: string | null
  onChange?: (def: PipelineDefinition) => void
  onSelectStep?: (stepId: string | null) => void
  /** Whether the work area hosting this canvas currently covers the viewport. */
  expanded?: boolean
  /** Supplied by that host; the canvas draws the control but does not own the state. */
  onToggleExpand?: () => void
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
  expanded = false,
  onToggleExpand,
}: {
  definition: PipelineDefinition
  statuses?: Record<string, string>
  editable?: boolean
  selectedStepId?: string | null
  onChange?: (def: PipelineDefinition) => void
  onSelectStep?: (stepId: string | null) => void
  /** Whether the work area hosting this canvas currently covers the viewport. */
  expanded?: boolean
  /** Supplied by that host; the canvas draws the control but does not own the state. */
  onToggleExpand?: () => void
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
  const surfaceRef = useRef<HTMLDivElement>(null)

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

  // Labels, statuses and selection are data-only: patch the nodes in place. A status
  // poll arrives every few seconds and usually changes nothing, so a node whose data
  // and selection both match is returned untouched — that identity is what lets the
  // `memo` on StepNode actually skip the render.
  useEffect(() => {
    const byId = new Map(definition.steps.map((s) => [s.id, s]))
    setNodes((prev) =>
      prev.map((n) => {
        const step = byId.get(n.id)
        if (!step) return n
        const selected = selectedStepId === n.id
        const next = nodeData(step, statuses?.[n.id])
        const data = sameNodeData(n.data, next) ? n.data : next
        if (data === n.data && selected === Boolean(n.selected)) return n
        return { ...n, selected, data, ariaLabel: data.ariaLabel }
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

  // `F` toggles, but only while the keyboard is already inside the canvas — the routes
  // that host it also host a YAML textarea and the inspector's fields, and a shortcut
  // that fires from there would be a trap rather than an accelerator.
  useEffect(() => {
    const surface = surfaceRef.current
    if (!surface) return
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "f" && e.key !== "F") return
      if (e.metaKey || e.ctrlKey || e.altKey) return
      const el = e.target as HTMLElement | null
      if (el?.isContentEditable) return
      if (el && ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName)) return
      e.preventDefault()
      onToggleExpand?.()
    }
    surface.addEventListener("keydown", onKeyDown)
    return () => surface.removeEventListener("keydown", onKeyDown)
  }, [onToggleExpand])

  // A viewport fitted to one box is wrong in the next, whether the canvas changed size
  // because it was expanded or because the window was dragged. React Flow can only fit
  // to dimensions it has already measured, so this waits for the element to actually
  // resize rather than fitting on a timer and landing on the old size.
  const expandedRef = useRef(expanded)
  expandedRef.current = expanded
  // A viewport the viewer panned or zoomed themselves is theirs; a resize must not
  // throw it away. Toggling full screen is the exception — that is a request for a new
  // view of the graph, so it fits regardless.
  const userMovedRef = useRef(false)
  const refitRef = useRef(false)

  const refit = useCallback(() => {
    refitRef.current = false
    userMovedRef.current = false
    void fitView({
      padding: expandedRef.current ? 0.16 : 0.28,
      maxZoom: expandedRef.current ? 1.25 : 1.05,
      duration: motionDuration(260),
    })
  }, [fitView])

  useEffect(() => {
    const surface = surfaceRef.current
    if (!surface) return
    // The observer reports the current size before any resize; that first call is the
    // box we are already fitted to.
    let settled = false
    let frame = 0
    const observer = new ResizeObserver(() => {
      if (!settled) {
        settled = true
        return
      }
      if (userMovedRef.current && !refitRef.current) return
      // A frame later React Flow's own observer has stored the new dimensions.
      window.cancelAnimationFrame(frame)
      frame = window.requestAnimationFrame(refit)
    })
    observer.observe(surface)
    return () => {
      observer.disconnect()
      window.cancelAnimationFrame(frame)
    }
  }, [refit])

  const wasExpanded = useRef(expanded)
  useEffect(() => {
    if (wasExpanded.current === expanded) return
    wasExpanded.current = expanded
    refitRef.current = true
    surfaceRef.current?.focus({ preventScroll: true })
    // Expanding usually resizes the canvas and the observer above takes it from there;
    // this covers the case where the box happens to come back the same size.
    const fallback = window.setTimeout(() => {
      if (refitRef.current) refit()
    }, 500)
    return () => window.clearTimeout(fallback)
  }, [expanded, refit])

  /** Throw away hand-placed positions and re-run the layered layout. */
  const relayout = () => {
    const next = layout(definition.steps, statuses, selectedStepId)
    setNodes(next.nodes)
    setEdges(next.edges)
    userMovedRef.current = false
    window.setTimeout(
      () => void fitView({ padding: 0.28, duration: motionDuration(220) }),
      0
    )
  }

  return (
    <div
      ref={surfaceRef}
      // Focusable only under the code above, so the canvas adds no stray tab stop but
      // can still own the keyboard once it has been expanded.
      tabIndex={-1}
      className={cn(canvasSurfaceClass, "outline-none")}
    >
      <div className="absolute top-3 right-3 z-10 flex gap-2">
        <button
          type="button"
          onClick={relayout}
          title="Tidy layout"
          className={toolbarButtonClass}
        >
          <LayoutGrid className="size-3.5" />
          Tidy
        </button>
        {onToggleExpand ? (
          <button
            type="button"
            onClick={onToggleExpand}
            aria-pressed={expanded}
            title={expanded ? "Exit full screen (Esc)" : "Full screen (F)"}
            className={toolbarButtonClass}
          >
            {expanded ? (
              <Minimize2 className="size-3.5" />
            ) : (
              <Maximize2 className="size-3.5" />
            )}
            {expanded ? "Exit" : "Full screen"}
          </button>
        ) : null}
        {editable ? (
          <button
            type="button"
            onClick={addStep}
            className={cn(toolbarButtonClass, "px-3 text-canvas-fg")}
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
        onMoveEnd={(event) => {
          if (event) userMovedRef.current = true
        }}
        onNodeClick={(_, node) => onSelectStep?.(node.id)}
        onPaneClick={() => onSelectStep?.(null)}
        nodeTypes={nodeTypes}
        aria-label={
          editable ? "Pipeline graph editor" : "Pipeline graph for this run"
        }
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
        // Hidden here rather than in CSS: one mechanism, and the library's own prop
        // is the supported way to do it. @xyflow/react is MIT; we run no Pro licence.
        proOptions={{ hideAttribution: true }}
      >
        <Background
          id="dots"
          variant={BackgroundVariant.Dots}
          color="var(--canvas-grid)"
          gap={16}
          size={1.5}
        />
        <Controls className="!border-canvas-border !bg-canvas-scrim !shadow-none [&>button]:!border-canvas-border [&>button]:!bg-transparent [&>button]:!text-canvas-fg-muted [&>button:hover]:!bg-canvas-raise [&>button:hover]:!text-canvas-fg" />
        <MiniMap
          className={cn(
            "!rounded-lg !border-canvas-border !bg-canvas-scrim sm:!block",
            // Too much of a small canvas to spend on an overview — but once the graph
            // owns the screen there is room for it at any width.
            expanded ? "!block" : "!hidden"
          )}
          style={{ width: 150, height: 104 }}
          nodeColor={(n) => {
            const data = n.data as StepNodeData | undefined
            if (n.selected) return "var(--canvas-accent)"
            return data?.status
              ? statusColor(data.status)
              : "var(--canvas-edge)"
          }}
          nodeStrokeWidth={2}
          nodeBorderRadius={3}
          maskColor="var(--canvas-mask)"
          pannable
          zoomable
        />
        {definition.steps.length === 0 ? (
          <Panel
            position="top-center"
            className="mt-16 rounded-lg border border-canvas-border bg-canvas-scrim px-4 py-3 text-center text-canvas-fg-muted text-xs backdrop-blur"
          >
            Empty pipeline — add a step or import fiber.yml
          </Panel>
        ) : null}
      </ReactFlow>
    </div>
  )
}
