import { createFileRoute, Link } from "@tanstack/react-router"
import { Ban, Copy, Download, ExternalLink } from "lucide-react"
import { useEffect, useMemo, useRef, useState } from "react"
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
  const [logs, setLogs] = useState<string[]>([])
  const [attempts, setAttempts] = useState<StepAttempt[]>([])
  const [selectedAttemptId, setSelectedAttemptId] = useState<string | null>(
    null
  )
  const [error, setError] = useState<string | null>(null)
  const [followLogs, setFollowLogs] = useState(true)
  const [copied, setCopied] = useState(false)
  const [tick, setTick] = useState(0)
  const stepsRef = useRef(steps)
  const selectedRef = useRef(selected)
  const logEndRef = useRef<HTMLDivElement | null>(null)
  const logViewportRef = useRef<HTMLDivElement | null>(null)
  stepsRef.current = steps
  selectedRef.current = selected

  const selectStep = (id: string | null) => {
    setSelected(id)
    setSelectedAttemptId(null)
    setFollowLogs(true)
    void navigate({
      search: (prev) => ({ ...prev, step: id ?? undefined }),
      replace: true,
    })
  }

  const definition = useMemo<PipelineDefinition | null>(() => {
    if (!run) return null
    const snap = run.definition_snapshot as {
      name?: string
      steps?: Array<{
        id: string
        name: string
        needs: string[]
        run: string
        image?: string
        labels?: string[]
        artifacts?: string[]
      }>
    }
    if (!snap?.steps) return null
    return {
      name: snap.name ?? "run",
      steps: snap.steps.map((s) => ({
        id: s.id,
        name: s.name,
        needs: s.needs ?? [],
        run: s.run,
        image: s.image,
        labels: s.labels,
        artifacts: s.artifacts,
      })),
    }
  }, [run])

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

  const restoreFailures = useMemo(
    () =>
      logs.filter(
        (l) =>
          l.includes("RESTORE FAILED") ||
          /\[system\].*restore .+ failed/i.test(l)
      ),
    [logs]
  )

  const selectedAttempt = useMemo(
    () => attempts.find((a) => a.id === selectedAttemptId) ?? null,
    [attempts, selectedAttemptId]
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
      ws = new WebSocket(api.runEventsUrl(runId))
      ws.onmessage = (ev) => {
        try {
          const msg = JSON.parse(ev.data as string) as {
            type: string
            status?: string
            step_id?: string
            step_run_id?: string
            data?: string
            stream?: string
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
          if (msg.type === "log" && msg.data && msg.step_run_id) {
            const cur = selectedRef.current
            const step = stepsRef.current.find(
              (s) => s.id === cur || s.step_id === cur
            )
            if (step && step.id === msg.step_run_id) {
              const line = `[${msg.stream ?? "out"}] ${msg.data}`
              setLogs((prev) => [...prev.slice(-800), line])
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
      if (retry) window.clearTimeout(retry)
      ws?.close()
    }
  }, [runId])

  useEffect(() => {
    const step = steps.find((s) => s.id === selected || s.step_id === selected)
    if (!step) {
      setAttempts([])
      setSelectedAttemptId(null)
      return
    }
    void api.listLogs(step.id).then((lines) => {
      setLogs(lines.map((l) => `[${l.stream}] ${l.data}`))
      setFollowLogs(true)
    })
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

  useEffect(() => {
    if (!followLogs) return
    logEndRef.current?.scrollIntoView({ block: "end" })
  }, [logs, followLogs])

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
        {isActiveStatus(run?.status) ? (
          <Button variant="outline" onClick={() => void cancel()}>
            <Ban className="size-4" />
            Cancel
          </Button>
        ) : null}
      </header>
      {error ? (
        <p className="px-6 pt-3 text-destructive text-sm">{error}</p>
      ) : null}
      <div className="grid min-h-0 flex-1 grid-cols-[1.1fr_0.9fr]">
        <div className="flex min-h-0 flex-col">
          <div className="min-h-[360px] flex-1 p-4">
            {definition ? (
              <DagCanvas
                definition={definition}
                statuses={statuses}
                editable={false}
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
          <div className="border-border/70 border-t px-4 py-3">
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
        <aside className="flex min-h-0 flex-col border-border/70 border-l">
          <div className="flex gap-1 overflow-x-auto border-border/60 border-b p-2">
            {steps.map((s) => (
              <button
                key={s.id}
                type="button"
                onClick={() => selectStep(s.id)}
                className={`rounded-md px-2 py-1 text-xs ${
                  selected === s.id ? "bg-sky-500/15" : "hover:bg-muted"
                }`}
              >
                <span
                  className="mr-1 inline-block size-1.5 rounded-full"
                  style={{ background: statusColor(s.status) }}
                />
                {s.step_name}
              </button>
            ))}
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
                    {restoreFailures.map((l, i) => (
                      <li key={i} className="break-all">
                        {l.replace(/^\[system\]\s*/, "")}
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
          <div className="flex items-center justify-between border-border/60 border-b px-3 py-1.5">
            <span className="text-[11px] text-muted-foreground">Logs</span>
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
            className="min-h-0 flex-1 overflow-auto bg-[oklch(0.12_0.01_260)] p-3 font-mono text-[11px] leading-relaxed"
          >
            {logs.length === 0 ? (
              <span className="text-white/30">Waiting for logs…</span>
            ) : (
              logs.map((l, i) => (
                <div
                  key={i}
                  className={`whitespace-pre-wrap ${logLineClass(l)}`}
                >
                  {l}
                </div>
              ))
            )}
            <div ref={logEndRef} />
          </div>
        </aside>
      </div>
    </AppShell>
  )
}
