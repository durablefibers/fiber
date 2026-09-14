import { Handle, type Node, type NodeProps, Position } from "@xyflow/react"
import { Boxes, GitFork, Package, RotateCcw, ShieldAlert } from "lucide-react"
import { memo } from "react"
import { statusColor } from "@/lib/api"
import { cn } from "@/lib/utils"

export type StepNodeData = {
  label: string
  stepId?: string
  status?: string
  labels?: string[]
  retries?: number
  artifacts?: string[]
  needsCount?: number
  runPreview?: string
  image?: string
  /** The step's `if:` expression, when it is not the default `success()`. */
  condition?: string
  continueOnError?: boolean
  /** Cells this step expands to at compile time, when it declares a matrix. */
  matrixCells?: number
  matrixAxes?: string[]
  /** Matrix bindings for an already-expanded cell, e.g. `os: linux`. */
  matrixBinding?: string
  /** What a screen reader announces for the node; built by `nodeAriaLabel`. */
  ariaLabel?: string
}

export type StepFlowNode = Node<StepNodeData, "step">

function StepNodeComponent({ data, selected }: NodeProps<StepFlowNode>) {
  const hasStatus = Boolean(data.status)
  const color = hasStatus ? statusColor(data.status!) : undefined
  const labels = (data.labels ?? []).slice(0, 2)
  const hasArtifacts = (data.artifacts?.length ?? 0) > 0
  const retries = data.retries ?? 0
  const showId = data.stepId && data.stepId !== data.label
  const running = data.status === "running"

  return (
    <div
      className={cn(
        "group min-w-[200px] max-w-[240px] rounded-xl border px-3.5 py-2.5 text-left transition-[border-color,box-shadow,background-color]",
        "bg-canvas-node shadow-[var(--canvas-node-shadow)]",
        selected
          ? "border-canvas-accent bg-canvas-node-selected shadow-[0_0_0_3px_var(--canvas-accent-ring)]"
          : "border-canvas-border hover:border-canvas-border-hover"
      )}
    >
      <Handle
        type="target"
        position={Position.Left}
        className="!-left-1.5 !size-2.5 !border-2 !border-canvas-node !bg-canvas-accent"
      />

      <div className="flex items-start gap-2.5">
        <span
          className={cn(
            "mt-1 inline-block size-2 shrink-0 rounded-full",
            !hasStatus && "bg-canvas-fg-faint",
            running && "motion-safe:animate-pulse"
          )}
          style={
            color
              ? { background: color, boxShadow: `0 0 10px ${color}` }
              : undefined
          }
        />
        <div className="min-w-0 flex-1">
          <div className="truncate font-semibold text-[13px] text-canvas-fg tracking-tight">
            {data.label}
          </div>
          {showId ? (
            <div className="truncate font-mono text-[11px] text-canvas-fg-subtle">
              {data.stepId}
            </div>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-1 text-canvas-fg-subtle">
          {data.continueOnError ? (
            <ShieldAlert className="size-3 text-canvas-warn" />
          ) : null}
          {retries > 0 ? <RotateCcw className="size-3" /> : null}
          {data.image ? <Boxes className="size-3" /> : null}
          {hasArtifacts ? (
            <Package className="size-3.5 text-canvas-artifact" />
          ) : null}
        </div>
      </div>

      {data.matrixBinding ? (
        <div className="mt-1.5 truncate rounded-md bg-canvas-matrix-bg px-1.5 py-0.5 font-mono text-[11px] text-canvas-matrix">
          {data.matrixBinding}
        </div>
      ) : null}

      {data.matrixCells ? (
        <div className="mt-1.5 inline-flex items-center gap-1 rounded-md bg-canvas-matrix-bg px-1.5 py-0.5 text-[11px] text-canvas-matrix">
          <GitFork className="size-3" />
          {data.matrixCells} cells
          {data.matrixAxes?.length ? (
            <span className="font-mono text-canvas-matrix opacity-75">
              {data.matrixAxes.join(", ")}
            </span>
          ) : null}
        </div>
      ) : null}

      {data.runPreview ? (
        <div className="mt-2 truncate rounded-md bg-canvas-inset px-2 py-1 font-mono text-[11px] text-canvas-fg-muted">
          {data.runPreview}
        </div>
      ) : null}

      <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1">
        {hasStatus ? (
          <span className="font-medium text-[11px] text-canvas-fg-muted uppercase tracking-wide">
            {data.status}
          </span>
        ) : null}
        {data.condition ? (
          <span
            className="truncate font-mono text-[11px] text-canvas-condition"
            title={data.condition}
          >
            if {data.condition}
          </span>
        ) : null}
      </div>

      {labels.length > 0 ? (
        <div className="mt-2 flex flex-wrap gap-1">
          {labels.map((l) => (
            <span
              key={l}
              className="rounded-md bg-canvas-chip px-1.5 py-0.5 font-mono text-[10px] text-canvas-fg-muted"
            >
              {l}
            </span>
          ))}
        </div>
      ) : null}

      <Handle
        type="source"
        position={Position.Right}
        className="!-right-1.5 !size-2.5 !border-2 !border-canvas-node !bg-canvas-accent"
      />
    </div>
  )
}

export const StepNode = memo(StepNodeComponent)
