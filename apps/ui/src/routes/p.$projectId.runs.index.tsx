import { createFileRoute, Link } from "@tanstack/react-router"
import { useEffect, useMemo, useState } from "react"
import { AppShell } from "@/components/app-shell"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  api,
  type Pipeline,
  type Project,
  type Run,
  statusColor,
} from "@/lib/api"
import { cn } from "@/lib/utils"

export const Route = createFileRoute("/p/$projectId/runs/")({
  component: RunsPage,
})

const PAGE = 25

const FILTERS = [
  { key: "all", label: "All" },
  { key: "running", label: "Running" },
  { key: "succeeded", label: "Succeeded" },
  { key: "failed", label: "Failed" },
] as const

type FilterKey = (typeof FILTERS)[number]["key"]

function relative(iso?: string): string {
  if (!iso) return "—"
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return "—"
  const s = Math.max(0, Math.round((Date.now() - t) / 1000))
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  return `${Math.floor(h / 24)}d ago`
}

function duration(run: Run): string {
  const start = run.started_at ?? run.created_at
  const end = run.finished_at
  if (!start || !end) return "—"
  const ms = new Date(end).getTime() - new Date(start).getTime()
  if (!Number.isFinite(ms) || ms < 0) return "—"
  if (ms < 1000) return `${ms}ms`
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s`
  return `${Math.floor(s / 60)}m ${s % 60}s`
}

function RunsPage() {
  const { projectId } = Route.useParams()
  const [project, setProject] = useState<Project | null>(null)
  const [pipelines, setPipelines] = useState<Pipeline[]>([])
  const [runs, setRuns] = useState<Run[]>([])
  const [cursor, setCursor] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [loadingMore, setLoadingMore] = useState(false)
  const [filter, setFilter] = useState<FilterKey>("all")
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    void (async () => {
      setLoading(true)
      try {
        const [p, pipes, page] = await Promise.all([
          api.getProject(projectId),
          api.listPipelines(projectId),
          api.listRuns(projectId, { limit: PAGE }),
        ])
        setProject(p)
        setPipelines(pipes)
        setRuns(page.items)
        setCursor(page.next_cursor)
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load runs")
      } finally {
        setLoading(false)
      }
    })()
  }, [projectId])

  const loadMore = async () => {
    if (!cursor) return
    setLoadingMore(true)
    try {
      const page = await api.listRuns(projectId, {
        limit: PAGE,
        before: cursor,
      })
      setRuns((prev) => [...prev, ...page.items])
      setCursor(page.next_cursor)
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not load more")
    } finally {
      setLoadingMore(false)
    }
  }

  const pipelineName = useMemo(() => {
    const byId = new Map(pipelines.map((p) => [p.id, p.name]))
    return (id: string) => byId.get(id) ?? "—"
  }, [pipelines])

  const shown = useMemo(
    () => (filter === "all" ? runs : runs.filter((r) => r.status === filter)),
    [runs, filter]
  )

  return (
    <AppShell projectId={projectId} projectName={project?.name}>
      <header className="flex shrink-0 flex-wrap items-center justify-between gap-3 border-border/70 border-b px-8 py-5">
        <div>
          <h1 className="font-semibold text-xl tracking-tight">Runs</h1>
          <p className="text-muted-foreground text-sm">
            Newest first. Every run keeps the pipeline definition it started
            with.
          </p>
        </div>
        <div className="flex gap-1 rounded-lg border border-border/60 p-0.5">
          {FILTERS.map((f) => (
            <button
              key={f.key}
              type="button"
              onClick={() => setFilter(f.key)}
              className={cn(
                "rounded-md px-2.5 py-1 font-medium text-xs transition",
                filter === f.key
                  ? "bg-sky-500/15 text-sky-300"
                  : "text-muted-foreground hover:bg-muted/60"
              )}
            >
              {f.label}
            </button>
          ))}
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">
        {error ? (
          <p className="mb-4 text-destructive text-sm">{error}</p>
        ) : null}
        {loading ? (
          <ul className="space-y-2">
            {[0, 1, 2, 3, 4].map((i) => (
              <li
                key={i}
                className="h-14 animate-pulse rounded-lg border border-border/50 bg-muted/20"
              />
            ))}
          </ul>
        ) : shown.length === 0 ? (
          <p className="text-muted-foreground text-sm">
            {runs.length === 0
              ? "No runs yet. Open a pipeline and press Run."
              : `No ${filter} runs on this page.`}
          </p>
        ) : (
          <ul className="space-y-1.5">
            {shown.map((r) => (
              <li key={r.id}>
                <Link
                  to="/p/$projectId/runs/$runId"
                  params={{ projectId, runId: r.id }}
                  className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-3 rounded-lg border border-border/60 px-4 py-3 transition hover:border-sky-500/40 hover:bg-card/60 sm:grid-cols-[auto_minmax(0,1fr)_auto_auto_auto]"
                >
                  <span
                    className="size-2 shrink-0 rounded-full"
                    style={{ background: statusColor(r.status) }}
                    title={r.status}
                  />
                  <span className="min-w-0">
                    <span className="block truncate font-medium text-sm">
                      {pipelineName(r.pipeline_id)}
                    </span>
                    <span className="block truncate font-mono text-[11px] text-muted-foreground">
                      {r.id.slice(0, 8)}
                      {r.retry_of ? " · retry" : ""}
                    </span>
                  </span>
                  <Badge variant="secondary" className="hidden sm:inline-flex">
                    {r.trigger}
                  </Badge>
                  <span className="hidden text-muted-foreground text-xs tabular-nums sm:block">
                    {duration(r)}
                  </span>
                  <span className="text-right text-muted-foreground text-xs">
                    {relative(r.created_at)}
                  </span>
                </Link>
              </li>
            ))}
          </ul>
        )}

        {cursor ? (
          <div className="mt-5 flex justify-center">
            <Button
              variant="outline"
              onClick={() => void loadMore()}
              disabled={loadingMore}
            >
              {loadingMore ? "Loading…" : "Load more"}
            </Button>
          </div>
        ) : null}
      </div>
    </AppShell>
  )
}
