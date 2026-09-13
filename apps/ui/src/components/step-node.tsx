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
        "bg-[oklch(0.18_0.014_260)] shadow-[0_10px_28px_rgba(0,0,0,0.32)]",
        selected
          ? "border-sky-400/90 bg-[oklch(0.20_0.02_250)] shadow-[0_0_0_3px_rgba(56,189,248,0.18)]"
          : "border-white/10 hover:border-white/22"
      )}
    >
      <Handle
        type="target"
        position={Position.Left}
        className="!-left-1.5 !size-2.5 !border-2 !border-[oklch(0.18_0.014_260)] !bg-sky-400/90"
      />

      <div className="flex items-start gap-2.5">
        <span
          className={cn(
            "mt-1 inline-block size-2 shrink-0 rounded-full",
            !hasStatus && "bg-white/25",
            running && "animate-pulse"
          )}
          style={
            color
              ? { background: color, boxShadow: `0 0 10px ${color}` }
              : undefined
          }
        />
        <div className="min-w-0 flex-1">
          <div className="truncate font-semibold text-[13px] text-white tracking-tight">
            {data.label}
          </div>
          {showId ? (
            <div className="truncate font-mono text-[10px] text-white/30">
              {data.stepId}
            </div>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-1 text-white/45">
          {data.continueOnError ? (
            <ShieldAlert className="size-3 text-amber-300/70" />
          ) : null}
          {retries > 0 ? <RotateCcw className="size-3" /> : null}
          {data.image ? <Boxes className="size-3" /> : null}
          {hasArtifacts ? (
            <Package className="size-3.5 text-amber-300/85" />
          ) : null}
        </div>
      </div>

      {data.matrixBinding ? (
        <div className="mt-1.5 truncate rounded-md bg-violet-400/12 px-1.5 py-0.5 font-mono text-[10px] text-violet-200/90">
          {data.matrixBinding}
        </div>
      ) : null}

      {data.matrixCells ? (
        <div className="mt-1.5 inline-flex items-center gap-1 rounded-md bg-violet-400/12 px-1.5 py-0.5 text-[10px] text-violet-200/90">
          <GitFork className="size-3" />
          {data.matrixCells} cells
          {data.matrixAxes?.length ? (
            <span className="font-mono text-violet-200/60">
              {data.matrixAxes.join(", ")}
            </span>
          ) : null}
        </div>
      ) : null}

      {data.runPreview ? (
        <div className="mt-2 truncate rounded-md bg-black/25 px-2 py-1 font-mono text-[10px] text-white/45">
          {data.runPreview}
        </div>
      ) : null}

      <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1">
        {hasStatus ? (
          <span className="font-medium text-[10px] text-white/40 uppercase tracking-wide">
            {data.status}
          </span>
        ) : null}
        {data.condition ? (
          <span
            className="truncate font-mono text-[10px] text-sky-200/60"
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
              className="rounded-md bg-white/6 px-1.5 py-0.5 font-mono text-[9px] text-white/50"
            >
              {l}
            </span>
          ))}
        </div>
      ) : null}

      <Handle
        type="source"
        position={Position.Right}
        className="!-right-1.5 !size-2.5 !border-2 !border-[oklch(0.18_0.014_260)] !bg-sky-400/90"
      />
    </div>
  )
}

export const StepNode = memo(StepNodeComponent)
