import { createFileRoute, Link } from "@tanstack/react-router"
import { useVirtualizer } from "@tanstack/react-virtual"
import { Ban, Copy, Download, ExternalLink, RotateCw } from "lucide-react"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { toast } from "sonner"
import { AppShell } from "@/components/app-shell"
import { DagCanvas } from "@/components/dag-canvas"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  type Artifact,
  api,
  type PipelineDefinition,
  type Project,
  type Run,
  type StepAttempt,
  type StepRun,
  statusColor,
} from "@/lib/api"
import { parseMatrixBindings } from "@/lib/dag-layout"
import {
  appendLogRows,
  type LogRow,
  MAX_LOG_ROWS,
  nextLogCursor,
  renderLogLine,
} from "@/lib/logs"
import { useExpand } from "@/lib/use-expand"
import { cn } from "@/lib/utils"

export const Route = createFileRoute("/p/$projectId/runs/$runId")({
  validateSearch: (search: Record<string, unknown>): { step?: string } => ({
    step:
      typeof search.step === "string" && search.step ? search.step : undefined,
  }),
  component: RunPage,
})

function formatBytes(n: number) {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}

function formatDuration(
  start?: string | null,
  end?: string | null
): string | null {
  if (!start) return null
  const a = new Date(start).getTime()
  const b = end ? new Date(end).getTime() : Date.now()
  if (Number.isNaN(a) || Number.isNaN(b) || b < a) return null
  const ms = b - a
  if (ms < 1000) return `${ms}ms`
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  const rem = s % 60
  if (m < 60) return `${m}m ${rem}s`
  const h = Math.floor(m / 60)
  return `${h}h ${m % 60}m`
}

/**
 * Lines the catch-up fetch asks for at a time, and how many pages it will walk.
 *
 * `MAX_LOG_ROWS` is all the viewer keeps, so one page is already more than the screen
 * can hold; the extra pages exist for the case where the gap is larger than a page and
 * the tail is what matters.
 */
const CATCH_UP_LIMIT = 500
const CATCH_UP_PAGES = 4
/** Fallback flush cadence where there is no animation frame to batch against. */
const LOG_FLUSH_MS = 50
/** Starting guess for a log row's height, in px; wrapped rows are measured for real. */
const LOG_ROW_ESTIMATE = 18

function isActiveStatus(status?: string) {
  return status === "pending" || status === "queued" || status === "running"
}

function logLineClass(line: string): string {
  if (line.includes("RESTORE FAILED") || /restore .+ failed/i.test(line)) {
    return "text-rose-300 font-medium"
  }
  if (line.startsWith("[stderr]")) return "text-rose-300/90"
  if (line.startsWith("[system]")) return "text-amber-200/80"
  return "text-emerald-100/90"
}

function RunPage() {
  const { projectId, runId } = Route.useParams()
  const { step: stepSearch } = Route.useSearch()
  const navigate = Route.useNavigate()
  const [project, setProject] = useState<Project | null>(null)
  const [run, setRun] = useState<Run | null>(null)
  const [steps, setSteps] = useState<StepRun[]>([])
  const [artifacts, setArtifacts] = useState<Artifact[]>([])
  const [selected, setSelected] = useState<string | null>(stepSearch ?? null)
  const [logs, setLogs] = useState<LogRow[]>([])
  const [attempts, setAttempts] = useState<StepAttempt[]>([])
  const [selectedAttemptId, setSelectedAttemptId] = useState<string | null>(
    null
  )
  const [error, setError] = useState<string | null>(null)
  const [followLogs, setFollowLogs] = useState(true)
  const [logFilter, setLogFilter] = useState("")
  const [wrapLogs, setWrapLogs] = useState(true)
  const [copied, setCopied] = useState(false)
  const [tick, setTick] = useState(0)
  const { expanded, toggle: toggleExpand } = useExpand()
  const stepsRef = useRef(steps)
  const selectedRef = useRef(selected)
  const logViewportRef = useRef<HTMLDivElement | null>(null)
  stepsRef.current = steps
  selectedRef.current = selected

  /**
   * The live log buffer's bookkeeping, all outside React state.
   *
   * A fast step publishes faster than React can render, so incoming lines are parked in
   * `pendingLogs` and committed once per animation frame rather than once per event.
   * `logCursor` is the highest `log_lines.id` the viewer holds: it is what a resync or a
   * reconnect refetches from, so a gap is closed by asking rather than by hoping the
   * next event arrives.
   */
  const pendingLogsRef = useRef<LogRow[]>([])
  const cancelFlushRef = useRef<(() => void) | null>(null)
  const logCursorRef = useRef<number | null>(null)
  /** Live lines held back while a catch-up fetch is in flight, so the two cannot
   * interleave and leave the buffer out of order. */
  const heldLogsRef = useRef<LogRow[]>([])
  const catchingUpRef = useRef(false)
  /** Bumped whenever the step or attempt on screen changes, so an in-flight catch-up
   * for the previous one cannot land in the new buffer. */
  const logGenRef = useRef(0)
  /** Keys for lines from a replica too old to send ids. Never deduplicated, only kept
   * distinct. */
  const synthIdRef = useRef(0)
  const selectedAttemptNumRef = useRef<number | undefined>(undefined)
  const connectedOnceRef = useRef(false)

  const selectStep = (id: string | null) => {
    setSelected(id)
    setSelectedAttemptId(null)
    setFollowLogs(true)
    void navigate({
      search: (prev) => ({ ...prev, step: id ?? undefined }),
      replace: true,
    })
  }

  /**
   * The canvas is built from the run's `step_runs`, not from the definition snapshot.
   * The snapshot holds what the author wrote; `step_runs` hold the compiled DAG, which
   * is what actually ran — so a matrix step appears as its real cells, with their own
   * statuses, needs and logs, instead of one node whose status never resolves.
   */
  const definition = useMemo<PipelineDefinition | null>(() => {
    const snap = run?.definition_snapshot as
      | {
          name?: string
          steps?: Array<{
            id: string
            name?: string
            artifacts?: string[]
            matrix?: Record<string, string[]>
            if?: string
            continue_on_error?: boolean
          }>
        }
      | undefined
    const name = snap?.name ?? "run"
    if (steps.length === 0) return null

    const authored = snap?.steps ?? []
    /**
     * Find the step the author wrote for a compiled step id. Matrix cells are
     * `<stepId>__<axis>_<value>`, so an id that is not itself authored belongs to the
     * *longest* authored id it extends — ids `build` and `build__docs` can both prefix
     * `build__docs__os_linux`, and only the longer one is its real parent.
     */
    const authoredFor = (stepId: string) => {
      const exact = authored.find((a) => a.id === stepId)
      if (exact) return exact
      let best: (typeof authored)[number] | undefined
      for (const a of authored) {
        if (!stepId.startsWith(`${a.id}__`)) continue
        if (!best || a.id.length > best.id.length) best = a
      }
      return best
    }

    return {
      name,
      steps: steps.map((s) => {
        const src = authoredFor(s.step_id)
        // The compiler renders a cell's bindings into the step name as `name (os: linux)`.
        const cell = src?.matrix
          ? /^(.*?)\s*\(([^()]*)\)\s*$/.exec(s.step_name)
          : null
        const bindings = cell
          ? parseMatrixBindings(cell[2], Object.keys(src?.matrix ?? {}))
          : {}
        const bound = Object.keys(bindings).length > 0
        return {
          id: s.step_id,
          name: cell && bound ? cell[1] : s.step_name,
          needs: s.needs ?? [],
          run: s.run_cmd,
          image: s.image,
          labels: s.labels,
          retries: s.retries,
          artifacts: src?.artifacts,
          matrix: bound ? bindings : undefined,
          if: src?.if,
          continue_on_error: src?.continue_on_error,
        }
      }),
    }
  }, [run, steps])

  const statuses = useMemo(() => {
    const m: Record<string, string> = {}
    for (const s of steps) m[s.step_id] = s.status
    return m
  }, [steps])

  const selectedStep = useMemo(
    () =>
      steps.find((s) => s.id === selected || s.step_id === selected) ?? null,
    [steps, selected]
  )

  const stepArtifacts = useMemo(() => {
    if (!selectedStep) return artifacts
    const mine = artifacts.filter((a) => a.step_run_id === selectedStep.id)
    return mine.length > 0 ? mine : artifacts
  }, [artifacts, selectedStep])

  const showingStepOnly =
    !!selectedStep && artifacts.some((a) => a.step_run_id === selectedStep.id)

  const shownLogs = useMemo(() => {
    const needle = logFilter.trim().toLowerCase()
    if (!needle) return logs
    return logs.filter((l) => l.text.toLowerCase().includes(needle))
  }, [logs, logFilter])

  const copyLogs = async () => {
    try {
      await navigator.clipboard.writeText(
        shownLogs.map((l) => l.text).join("\n")
      )
      toast.success(`Copied ${shownLogs.length} lines`)
    } catch {
      toast.error("Clipboard unavailable")
    }
  }

  const restoreFailures = useMemo(
    () =>
      logs.filter(
        (l) =>
          l.text.includes("RESTORE FAILED") ||
          /\[system\].*restore .+ failed/i.test(l.text)
      ),
    [logs]
  )

  const selectedAttempt = useMemo(
    () => attempts.find((a) => a.id === selectedAttemptId) ?? null,
    [attempts, selectedAttemptId]
  )
  // The WS handler runs outside React state; it needs to know whether the newest
  // attempt is the one on screen.
  const viewingLatestAttemptRef = useRef(true)
  viewingLatestAttemptRef.current =
    attempts.length === 0 ||
    selectedAttemptId === attempts[attempts.length - 1]?.id
  selectedAttemptNumRef.current = selectedAttempt?.attempt

  /** Commit whatever has accumulated since the last frame. */
  const flushLogs = useCallback(() => {
    cancelFlushRef.current = null
    const batch = pendingLogsRef.current
    if (batch.length === 0) return
    pendingLogsRef.current = []
    setLogs((prev) => appendLogRows(prev, batch))
  }, [])

  /**
   * Queue lines for the next frame.
   *
   * One `setLogs` per event copied the whole buffer and re-rendered every row, which put
   * the ceiling on live viewing at a couple of hundred lines a second — well under what
   * a build produces. An animation frame coalesces a burst into one render; a hidden tab
   * gets no frames at all, so the queue is capped at what the buffer would keep anyway.
   */
  const queueLogs = useCallback(
    (rows: LogRow[]) => {
      if (rows.length === 0) return
      const pending = pendingLogsRef.current
      pending.push(...rows)
      if (pending.length > MAX_LOG_ROWS) {
        pending.splice(0, pending.length - MAX_LOG_ROWS)
      }
      const last = rows[rows.length - 1]
      if (last.id > 0) logCursorRef.current = last.id
      if (cancelFlushRef.current) return
      if (typeof requestAnimationFrame === "function") {
        const handle = requestAnimationFrame(() => flushLogs())
        cancelFlushRef.current = () => cancelAnimationFrame(handle)
      } else {
        const handle = window.setTimeout(() => flushLogs(), LOG_FLUSH_MS)
        cancelFlushRef.current = () => window.clearTimeout(handle)
      }
    },
    [flushLogs]
  )

  /** Drop everything the buffer holds and abandon any catch-up still in flight. */
  const resetLogs = useCallback(() => {
    logGenRef.current += 1
    catchingUpRef.current = false
    cancelFlushRef.current?.()
    cancelFlushRef.current = null
    pendingLogsRef.current = []
    heldLogsRef.current = []
    logCursorRef.current = null
    setLogs([])
  }, [])

  /**
   * Close a gap by asking the server, from the last id the viewer holds.
   *
   * Runs when the server says the stream lagged (`resync`) and when the socket comes
   * back after a drop: in both cases events were published that this tab never saw, and
   * nothing will republish them. Live lines are held back while it runs so that a page
   * of older ids cannot arrive after a newer line and be discarded as already seen.
   */
  const catchUpLogs = useCallback(async () => {
    const cur = selectedRef.current
    const step = stepsRef.current.find((s) => s.id === cur || s.step_id === cur)
    if (!step || !viewingLatestAttemptRef.current || catchingUpRef.current) {
      return
    }
    const gen = logGenRef.current
    const attempt = selectedAttemptNumRef.current
    catchingUpRef.current = true
    try {
      let cursor = logCursorRef.current
      for (let page = 0; page < CATCH_UP_PAGES; page++) {
        const lines = await api.listLogs(step.id, {
          attempt,
          afterId: cursor ?? undefined,
          limit: CATCH_UP_LIMIT,
        })
        if (gen !== logGenRef.current) return
        if (lines.length === 0) break
        queueLogs(
          lines.map((l) => ({
            id: l.id,
            text: renderLogLine(l.stream, l.data),
          }))
        )
        // The page is ordered by id and its last line is the next `after_id`.
        cursor = nextLogCursor(lines, cursor)
        if (lines.length < CATCH_UP_LIMIT) break
      }
    } catch {
      // Leave the cursor where it is; the next resync or reconnect tries again.
    } finally {
      if (gen === logGenRef.current) {
        catchingUpRef.current = false
        // Behind the fetched pages, and deduplicated against them by id.
        queueLogs(heldLogsRef.current)
        heldLogsRef.current = []
      }
    }
  }, [queueLogs])

  // Nothing should still be scheduled once the page is gone.
  useEffect(
    () => () => {
      cancelFlushRef.current?.()
      cancelFlushRef.current = null
    },
    []
  )

  const refreshArtifacts = async () => {
    try {
      setArtifacts(await api.listArtifacts(runId))
    } catch {
      /* ignore while loading */
    }
  }

  useEffect(() => {
    if (!isActiveStatus(run?.status)) return
    const id = window.setInterval(() => setTick((t) => t + 1), 1000)
    return () => window.clearInterval(id)
  }, [run?.status])

  useEffect(() => {
    void (async () => {
      try {
        const [p, detail] = await Promise.all([
          api.getProject(projectId),
          api.getRun(runId),
        ])
        setProject(p)
        setRun(detail.run)
        setSteps(detail.steps)
        const fromSearch = stepSearch
          ? detail.steps.find(
              (s) => s.id === stepSearch || s.step_id === stepSearch
            )?.id
          : undefined
        const nextId = fromSearch ?? detail.steps[0]?.id ?? null
        setSelected(nextId)
        if (nextId && !stepSearch) {
          void navigate({
            search: (prev) => ({ ...prev, step: nextId }),
            replace: true,
          })
        } else if (
          fromSearch &&
          stepSearch &&
          stepSearch !== fromSearch &&
          stepSearch !== nextId
        ) {
          void navigate({
            search: (prev) => ({ ...prev, step: nextId }),
            replace: true,
          })
        }
        await refreshArtifacts()
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load")
      }
    })()
  }, [projectId, runId])

  useEffect(() => {
    let ws: WebSocket | null = null
    let closed = false
    let retry: number | undefined

    const connect = () => {
      if (closed) return
      ws = new WebSocket(api.runEventsUrl(runId), api.runEventsProtocols())
      ws.onopen = () => {
        // A reconnect means a window where events went nowhere. The first connect is
        // not one: the log-fetch effect below is already loading the step.
        if (connectedOnceRef.current) void catchUpLogs()
        connectedOnceRef.current = true
      }
      ws.onmessage = (ev) => {
        try {
          const msg = JSON.parse(ev.data as string) as {
            type: string
            status?: string
            step_id?: string
            step_run_id?: string
            data?: string
            stream?: string
            lines?: { id?: number; stream?: string; data?: string }[]
          }
          if (msg.type === "run_updated" && msg.status) {
            setRun((r) => (r ? { ...r, status: msg.status! } : r))
            if (
              msg.status === "succeeded" ||
              msg.status === "failed" ||
              msg.status === "cancelled"
            ) {
              void refreshArtifacts()
            }
          }
          if (msg.type === "step_updated" && msg.step_id && msg.status) {
            setSteps((prev) =>
              prev.map((s) =>
                s.step_id === msg.step_id
                  ? {
                      ...s,
                      status: msg.status!,
                      finished_at:
                        msg.status === "running"
                          ? s.finished_at
                          : (s.finished_at ?? new Date().toISOString()),
                      started_at:
                        msg.status === "running" && !s.started_at
                          ? new Date().toISOString()
                          : s.started_at,
                    }
                  : s
              )
            )
            if (msg.status === "succeeded" || msg.status === "failed") {
              void refreshArtifacts()
            }
          }
          // `log` is one line, `log_batch` is many in one frame — the agent coalesces
          // its output and the server stores and publishes it a batch at a time. A
          // replica older than the batch still publishes single lines, so both arrive
          // during a rolling deploy.
          // The server dropped events this viewer had not read yet — a slow tab, or a
          // burst larger than its queue. The socket stays open; what is missing is
          // fetched by id rather than guessed at.
          if (msg.type === "resync") {
            void catchUpLogs()
          }
          if (msg.type === "log" || msg.type === "log_batch") {
            const incoming =
              msg.type === "log"
                ? msg.data
                  ? [{ stream: msg.stream, data: msg.data }]
                  : []
                : (msg.lines ?? [])
            const cur = selectedRef.current
            const step = stepsRef.current.find(
              (s) => s.id === cur || s.step_id === cur
            )
            // Live lines belong to the newest attempt; appending them while an older
            // one is on screen would mix two attempts together.
            if (
              step &&
              step.id === msg.step_run_id &&
              viewingLatestAttemptRef.current &&
              incoming.length > 0
            ) {
              const rendered = incoming
                .filter((l) => l.data !== undefined)
                .map((l) => ({
                  // A replica older than `log_batch` sends no id; it only needs to be
                  // a distinct React key, and it must never look newer than a real one.
                  id: l.id ?? --synthIdRef.current,
                  text: renderLogLine(l.stream, l.data as string),
                }))
              if (catchingUpRef.current) heldLogsRef.current.push(...rendered)
              else queueLogs(rendered)
            }
          }
        } catch {
          /* ignore */
        }
      }
      ws.onclose = () => {
        if (closed) return
        retry = window.setTimeout(connect, 2000)
      }
    }
    connect()
    return () => {
      closed = true
      connectedOnceRef.current = false
      if (retry) window.clearTimeout(retry)
      ws?.close()
    }
  }, [runId, catchUpLogs, queueLogs])

  useEffect(() => {
    const step = steps.find((s) => s.id === selected || s.step_id === selected)
    if (!step) {
      setAttempts([])
      setSelectedAttemptId(null)
      return
    }
    void api
      .listStepAttempts(step.id)
      .then((rows) => {
        setAttempts(rows)
        setSelectedAttemptId((prev) =>
          prev && rows.some((r) => r.id === prev)
            ? prev
            : (rows[rows.length - 1]?.id ?? null)
        )
      })
      .catch(() => {
        setAttempts([])
        setSelectedAttemptId(null)
      })
  }, [selected, steps.length])

  // Logs are fetched per attempt: `seq` restarts each attempt, so a retried step's
  // output would otherwise interleave. Refetches when the viewer picks another attempt.
  //
  // Deliberately *not* keyed on `steps`: that array gets a new identity on every
  // `step_updated`, so a 50-cell matrix refetched the selected step's log a hundred
  // times over a run. The step run's id is what actually decides which log to load.
  const selectedStepId = selectedStep?.id
  const selectedAttemptNum = selectedAttempt?.attempt
  useEffect(() => {
    resetLogs()
    if (!selectedStepId) return
    const gen = logGenRef.current
    void api
      .listLogs(selectedStepId, { attempt: selectedAttemptNum })
      .then((lines) => {
        if (gen !== logGenRef.current) return
        setLogs(
          lines.map((l) => ({
            id: l.id,
            text: renderLogLine(l.stream, l.data),
          }))
        )
        // The page's last line is where a later resync resumes from.
        logCursorRef.current = nextLogCursor(lines, null)
        setFollowLogs(true)
      })
      .catch((e) => {
        if (gen === logGenRef.current) {
          setError(e instanceof Error ? e.message : "Could not load logs")
        }
      })
  }, [selectedStepId, selectedAttemptNum, resetLogs])

  // Refresh attempts when step reaches a terminal status via WS.
  useEffect(() => {
    if (!selectedStep) return
    if (
      !["succeeded", "failed", "cancelled", "skipped"].includes(
        selectedStep.status
      )
    ) {
      return
    }
    void api
      .listStepAttempts(selectedStep.id)
      .then(setAttempts)
      .catch(() => setAttempts([]))
  }, [selectedStep?.id, selectedStep?.status, selectedStep?.attempt])

  /**
   * Only the rows on screen are in the DOM.
   *
   * A build can leave 800 lines in the buffer, and re-laying all of them out on every
   * frame is what made a fast step unwatchable. Rows are measured rather than assumed:
   * with wrapping on, a long line is several rows tall.
   */
  const logVirtualizer = useVirtualizer({
    count: shownLogs.length,
    getScrollElement: () => logViewportRef.current,
    estimateSize: () => LOG_ROW_ESTIMATE,
    overscan: 24,
    // The log id, so a row keeps its identity as the buffer's head is trimmed.
    getItemKey: (index) => shownLogs[index]?.id ?? index,
  })

  useEffect(() => {
    if (!followLogs) return
    // Scroll the log viewport itself rather than `scrollIntoView`, which walks up to
    // the nearest scrollable ancestor — when the panes stack on a narrow screen that
    // is the page, and following the tail would drag the canvas out of view.
    if (shownLogs.length > 0) {
      logVirtualizer.scrollToIndex(shownLogs.length - 1, { align: "end" })
      return
    }
    const el = logViewportRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [shownLogs, followLogs, logVirtualizer])

  const onLogScroll = () => {
    const el = logViewportRef.current
    if (!el) return
    const dist = el.scrollHeight - el.scrollTop - el.clientHeight
    setFollowLogs(dist < 48)
  }

  const cancel = async () => {
    try {
      const r = await api.cancelRun(runId)
      setRun(r)
    } catch (e) {
      setError(e instanceof Error ? e.message : "Cancel failed")
    }
  }

  const retry = async (failedOnly: boolean) => {
    try {
      const { run: created } = await api.retryRun(runId, failedOnly)
      void navigate({
        to: "/p/$projectId/runs/$runId",
        params: { projectId, runId: created.id },
      })
    } catch (e) {
      setError(e instanceof Error ? e.message : "Retry failed")
    }
  }

  const download = async (a: Artifact) => {
    try {
      await api.downloadArtifact(a.id, a.name)
    } catch (e) {
      setError(e instanceof Error ? e.message : "Download failed")
    }
  }

  const copyRunId = async () => {
    try {
      await navigator.clipboard.writeText(runId)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    } catch {
      /* ignore */
    }
  }

  const runDuration = formatDuration(
    run?.started_at ?? run?.created_at,
    run?.finished_at ??
      (isActiveStatus(run?.status) ? new Date().toISOString() : null)
  )
  // Re-render live duration while the run is active.
  void tick

  const stepDuration = selectedStep
    ? formatDuration(
        selectedStep.started_at,
        selectedStep.finished_at ??
          (isActiveStatus(selectedStep.status)
            ? new Date().toISOString()
            : null)
      )
    : null

  const pipelineName =
    (run?.definition_snapshot as { name?: string } | undefined)?.name ??
    "Pipeline"
  // Cancel and retry both require Writer (routes.rs: require_run(.., Writer)). Offering
  // them to a reader only produces a 403 after the click.
  const canRun = project !== null && project.role !== "reader"

  return (
    <AppShell projectId={projectId} projectName={project?.name}>
      <header className="flex items-center justify-between gap-4 border-border/70 border-b px-6 py-4">
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="font-mono text-sm">{runId.slice(0, 8)}</h1>
            <button
              type="button"
              onClick={() => void copyRunId()}
              className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-muted hover:text-foreground"
              title="Copy full run id"
            >
              <Copy className="size-3" />
              {copied ? "copied" : "copy"}
            </button>
            <Badge
              style={{
                background: statusColor(run?.status ?? "pending"),
                color: "#fff",
              }}
            >
              {run?.status ?? "…"}
            </Badge>
            {runDuration ? (
              <span className="text-muted-foreground text-xs tabular-nums">
                {runDuration}
                {isActiveStatus(run?.status) ? "…" : ""}
              </span>
            ) : null}
            <span className="text-muted-foreground text-xs">
              {run?.trigger}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
            {run?.pipeline_id ? (
              <Link
                to="/p/$projectId/pipelines/$pipelineId"
                params={{ projectId, pipelineId: run.pipeline_id }}
                className="inline-flex items-center gap-1 hover:text-foreground"
              >
                <ExternalLink className="size-3" />
                {pipelineName}
              </Link>
            ) : null}
            {(() => {
              const workspace = (
                run?.definition_snapshot as
                  | { workspace?: { repo?: string; ref?: string } }
                  | undefined
              )?.workspace
              if (!workspace?.repo) return null
              return (
                <span className="truncate font-mono">
                  {workspace.repo}@{workspace.ref ?? "main"}
                </span>
              )
            })()}
          </div>
        </div>
        {!canRun ? null : isActiveStatus(run?.status) ? (
          <Button variant="outline" onClick={() => void cancel()}>
            <Ban className="size-4" />
            Cancel
          </Button>
        ) : (
          <div className="flex gap-2">
            <Button variant="outline" onClick={() => void retry(false)}>
              <RotateCw className="size-4" />
              Re-run
            </Button>
            {run?.status === "failed" ? (
              <Button variant="outline" onClick={() => void retry(true)}>
                <RotateCw className="size-4" />
                Re-run failed steps
              </Button>
            ) : null}
          </div>
        )}
      </header>
      {error ? (
        <p className="px-6 pt-3 text-destructive text-sm">{error}</p>
      ) : null}
      {/*
        `min-h-0` is what lets the two panes scroll independently side by side, but it
        also zeroes their contribution to grid row sizing — stacked in one column the
        rows would split the height evenly and the content would spill over the pane
        below. So the panes only go min-height-0 once they are actually side by side.
      */}
      {/*
        Expanded, the run takes the viewport: the graph gets the room the artifact list
        was using, and the step rail and log stream stay beside it so clicking a node
        still lands on its logs — which is the reason to open the graph up at all.
      */}
      <div
        className={cn(
          "grid min-h-0 flex-1 grid-cols-1 overflow-y-auto lg:grid-cols-[1.1fr_0.9fr] lg:overflow-hidden",
          // A wide DAG wants width above all else, so expanded the log pane stops
          // scaling with the window and the graph takes everything else.
          // Stacked, the rows would size to content and hand the canvas the smaller
          // half — the opposite of what expanding it was for.
          expanded &&
            "fixed inset-0 z-50 grid-rows-[1.15fr_0.85fr] overflow-hidden bg-background lg:grid-cols-[minmax(0,1fr)_380px] lg:grid-rows-none"
        )}
      >
        <div className={cn("flex flex-col lg:min-h-0", expanded && "min-h-0")}>
          <div
            className={cn(
              "h-[380px] shrink-0 p-4 lg:h-auto lg:min-h-[360px] lg:flex-1",
              expanded && "h-auto min-h-0 flex-1"
            )}
          >
            {definition ? (
              <DagCanvas
                definition={definition}
                statuses={statuses}
                editable={false}
                expanded={expanded}
                onToggleExpand={toggleExpand}
                selectedStepId={
                  steps.find((s) => s.id === selected)?.step_id ?? selected
                }
                onSelectStep={(stepId) => {
                  if (!stepId) {
                    selectStep(null)
                    return
                  }
                  const match = steps.find((s) => s.step_id === stepId)
                  selectStep(match?.id ?? stepId)
                }}
              />
            ) : null}
          </div>
          <div
            className={cn(
              "border-border/70 border-t px-4 py-3",
              expanded && "hidden"
            )}
          >
            <div className="mb-2 flex items-center justify-between">
              <h2 className="font-medium text-sm">
                Artifacts
                {showingStepOnly && selectedStep ? (
                  <span className="ml-2 font-normal text-muted-foreground text-xs">
                    · {selectedStep.step_name}
                  </span>
                ) : null}
              </h2>
              <Button
                variant="ghost"
                size="sm"
                className="h-7 text-xs"
                onClick={() => void refreshArtifacts()}
              >
                Refresh
              </Button>
            </div>
            {stepArtifacts.length === 0 ? (
              <p className="text-muted-foreground text-xs">
                No artifacts yet. Declare paths on a step to upload after
                success.
              </p>
            ) : (
              <ul className="max-h-40 space-y-1 overflow-y-auto">
                {stepArtifacts.map((a) => (
                  <li
                    key={a.id}
                    className="flex items-center justify-between gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-muted/60"
                  >
                    <div className="min-w-0">
                      <div className="truncate font-mono text-xs">{a.name}</div>
                      <div className="text-[11px] text-muted-foreground">
                        {formatBytes(a.size)}
                      </div>
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-7 shrink-0"
                      onClick={() => void download(a)}
                    >
                      <Download className="size-3.5" />
                      Download
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
        <aside
          className={cn(
            "flex flex-col border-border/70 border-t lg:min-h-0 lg:border-t-0 lg:border-l",
            expanded && "min-h-0"
          )}
        >
          <div className="flex max-h-28 shrink-0 flex-wrap gap-1 overflow-y-auto border-border/60 border-b p-2">
            {steps.map((s) => (
              <button
                key={s.id}
                type="button"
                onClick={() => selectStep(s.id)}
                title={`${s.step_name} · ${s.status}`}
                className={`max-w-full truncate rounded-md px-2 py-1 text-xs transition ${
                  selected === s.id
                    ? "bg-sky-500/15 text-sky-100"
                    : "text-muted-foreground hover:bg-muted hover:text-foreground"
                }`}
              >
                <span
                  className="mr-1 inline-block size-1.5 rounded-full align-middle"
                  style={{ background: statusColor(s.status) }}
                />
                {s.step_name}
              </button>
            ))}
            {steps.length === 0 ? (
              <span className="px-1 py-1 text-muted-foreground text-xs">
                No steps.
              </span>
            ) : null}
          </div>
          {selectedStep ? (
            <div className="space-y-1.5 border-border/60 border-b px-3 py-2 text-[11px] text-muted-foreground">
              <div className="flex flex-wrap gap-x-3 gap-y-1">
                <span className="text-foreground capitalize">
                  {selectedStep.status}
                </span>
                <span>
                  attempt {selectedStep.attempt}
                  {selectedStep.retries > 0
                    ? ` / ${selectedStep.retries + 1}`
                    : ""}
                </span>
                {selectedStep.exit_code != null ? (
                  <span>exit {selectedStep.exit_code}</span>
                ) : null}
                {stepDuration ? (
                  <span className="tabular-nums">
                    {stepDuration}
                    {isActiveStatus(selectedStep.status) ? "…" : ""}
                  </span>
                ) : null}
              </div>
              {selectedStep.run_cmd ? (
                <pre className="max-h-16 overflow-auto whitespace-pre-wrap rounded bg-muted/40 px-2 py-1 font-mono text-[10px] text-foreground/80">
                  {selectedStep.run_cmd}
                </pre>
              ) : null}
              {selectedStep.error ? (
                <p className="text-rose-400">{selectedStep.error}</p>
              ) : null}
              {restoreFailures.length > 0 ? (
                <div className="rounded border border-rose-500/40 bg-rose-500/10 px-2 py-1.5 text-[11px] text-rose-200">
                  <div className="font-medium">Artifact restore failed</div>
                  <ul className="mt-1 space-y-0.5 font-mono text-[10px] text-rose-100/90">
                    {restoreFailures.map((l) => (
                      <li key={l.id} className="break-all">
                        {l.text.replace(/^\[system\]\s*/, "")}
                      </li>
                    ))}
                  </ul>
                </div>
              ) : null}
              {attempts.length > 0 ? (
                <div className="pt-1">
                  <div className="mb-1 text-[10px] text-muted-foreground/80 uppercase tracking-wide">
                    Attempts · click for details
                  </div>
                  <ul className="max-h-32 space-y-1 overflow-y-auto">
                    {attempts.map((a) => {
                      const dur = formatDuration(a.started_at, a.finished_at)
                      const active = selectedAttemptId === a.id
                      return (
                        <li key={a.id}>
                          <button
                            type="button"
                            onClick={() =>
                              setSelectedAttemptId(active ? null : a.id)
                            }
                            className={`flex w-full flex-wrap items-center gap-x-2 gap-y-0.5 rounded px-1.5 py-1 text-left font-mono text-[10px] ${
                              active
                                ? "bg-sky-500/20 ring-1 ring-sky-500/40"
                                : "bg-muted/30 hover:bg-muted/50"
                            }`}
                          >
                            <span className="text-foreground">
                              #{a.attempt}
                            </span>
                            <span
                              className="capitalize"
                              style={{ color: statusColor(a.status) }}
                            >
                              {a.status}
                            </span>
                            {a.exit_code != null ? (
                              <span>exit {a.exit_code}</span>
                            ) : null}
                            {dur ? (
                              <span className="text-muted-foreground tabular-nums">
                                {dur}
                              </span>
                            ) : null}
                            {a.agent_id ? (
                              <span
                                className="text-muted-foreground"
                                title={a.agent_id}
                              >
                                agent {a.agent_id.slice(0, 8)}
                              </span>
                            ) : null}
                          </button>
                        </li>
                      )
                    })}
                  </ul>
                  {selectedAttempt ? (
                    <div className="mt-1.5 space-y-1 rounded border border-border/60 bg-muted/20 px-2 py-1.5 text-[10px]">
                      <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-muted-foreground">
                        <span>
                          started{" "}
                          <span className="text-foreground">
                            {new Date(
                              selectedAttempt.started_at
                            ).toLocaleString()}
                          </span>
                        </span>
                        {selectedAttempt.finished_at ? (
                          <span>
                            finished{" "}
                            <span className="text-foreground">
                              {new Date(
                                selectedAttempt.finished_at
                              ).toLocaleString()}
                            </span>
                          </span>
                        ) : (
                          <span className="text-amber-200/90">in flight</span>
                        )}
                      </div>
                      {selectedAttempt.agent_id ? (
                        <div className="break-all font-mono text-muted-foreground">
                          agent{" "}
                          <span className="text-foreground">
                            {selectedAttempt.agent_id}
                          </span>
                        </div>
                      ) : null}
                      {selectedAttempt.error ? (
                        <p className="whitespace-pre-wrap text-rose-400">
                          {selectedAttempt.error}
                        </p>
                      ) : null}
                      {!selectedAttempt.error &&
                      selectedAttempt.status === "failed" ? (
                        <p className="text-muted-foreground">
                          No error string stored — check logs below.
                        </p>
                      ) : null}
                    </div>
                  ) : null}
                </div>
              ) : null}
            </div>
          ) : null}
          <div className="flex shrink-0 flex-wrap items-center gap-2 border-border/60 border-b px-3 py-1.5">
            <span className="text-[11px] text-muted-foreground">
              Logs
              {logFilter ? (
                <span className="ml-1 tabular-nums">
                  {shownLogs.length}/{logs.length}
                </span>
              ) : logs.length ? (
                <span className="ml-1 tabular-nums">{logs.length}</span>
              ) : null}
            </span>
            <input
              value={logFilter}
              onChange={(e) => setLogFilter(e.target.value)}
              placeholder="Filter…"
              aria-label="Filter log lines"
              className="h-6 min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 text-[11px] outline-none focus:border-sky-500/50"
            />
            <button
              type="button"
              className="text-[11px] text-muted-foreground hover:text-foreground"
              onClick={() => setWrapLogs((v) => !v)}
              title="Toggle line wrapping"
            >
              {wrapLogs ? "wrap" : "nowrap"}
            </button>
            <button
              type="button"
              className="text-[11px] text-muted-foreground hover:text-foreground"
              onClick={() => void copyLogs()}
              title="Copy visible lines"
            >
              copy
            </button>
            <button
              type="button"
              className="text-[11px] text-muted-foreground hover:text-foreground"
              onClick={() => setFollowLogs(true)}
            >
              {followLogs ? "following" : "follow"}
            </button>
          </div>
          <div
            ref={logViewportRef}
            onScroll={onLogScroll}
            className={`min-h-[240px] flex-1 overflow-auto bg-[oklch(0.12_0.01_260)] p-3 font-mono text-[11px] leading-relaxed lg:min-h-0 ${
              wrapLogs ? "" : "whitespace-nowrap"
            }`}
          >
            {logs.length === 0 ? (
              <span className="text-white/30">Waiting for logs…</span>
            ) : shownLogs.length === 0 ? (
              <span className="text-white/30">
                No lines match “{logFilter}”.
              </span>
            ) : (
              <div
                className="relative w-full"
                style={{ height: logVirtualizer.getTotalSize() }}
              >
                {logVirtualizer.getVirtualItems().map((item) => {
                  const line = shownLogs[item.index]
                  if (!line) return null
                  return (
                    <div
                      key={item.key}
                      data-index={item.index}
                      ref={logVirtualizer.measureElement}
                      className={`absolute top-0 left-0 ${wrapLogs ? "whitespace-pre-wrap" : "whitespace-pre"} ${logLineClass(line.text)}`}
                      style={{
                        transform: `translateY(${item.start}px)`,
                        // `max-content` keeps the viewport's horizontal scroll working
                        // when wrapping is off, where a row is wider than the pane.
                        width: wrapLogs ? "100%" : "max-content",
                        minWidth: "100%",
                      }}
                    >
                      {line.text}
                    </div>
                  )
                })}
              </div>
            )}
          </div>
        </aside>
      </div>
    </AppShell>
  )
}
