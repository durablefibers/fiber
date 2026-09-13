import { createFileRoute, Link, useRouter } from "@tanstack/react-router"
import { ArrowRight, GitBranch, Plus, Sliders } from "lucide-react"
import { useEffect, useState } from "react"
import { AppShell } from "@/components/app-shell"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  api,
  type Pipeline,
  type Project,
  type Run,
  statusColor,
} from "@/lib/api"
import { PIPELINE_TEMPLATES, type PipelineTemplate } from "@/lib/templates"

export const Route = createFileRoute("/p/$projectId/")({
  component: ProjectPage,
})

const RECENT_RUNS = 12

function pipelineBlurb(p: Pipeline): string {
  const n = p.definition.steps?.length ?? 0
  const bits = [`${n} step${n === 1 ? "" : "s"}`]
  if (p.definition.on?.cron) {
    bits.push(`cron:${p.definition.on.cron}`)
  } else if (p.definition.on?.interval_minutes) {
    bits.push(`every ${p.definition.on.interval_minutes}m`)
  }
  if (p.definition.on?.push?.branches?.length) {
    bits.push(`push:${p.definition.on.push.branches.join(",")}`)
  }
  if (p.definition.workspace?.repo) bits.push("git workspace")
  const maxNeeds = Math.max(
    0,
    ...(p.definition.steps ?? []).map((s) => s.needs?.length ?? 0)
  )
  if (maxNeeds >= 2) bits.push("fan-in")
  if ((p.definition.steps ?? []).some((s) => s.matrix)) bits.push("matrix")
  return bits.join(" · ")
}

function relative(iso?: string): string {
  if (!iso) return ""
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return ""
  const s = Math.max(0, Math.round((Date.now() - t) / 1000))
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  return `${Math.floor(h / 24)}d ago`
}

function ProjectPage() {
  const { projectId } = Route.useParams()
  const router = useRouter()
  const [project, setProject] = useState<Project | null>(null)
  const [pipelines, setPipelines] = useState<Pipeline[]>([])
  const [runs, setRuns] = useState<Run[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [pickerOpen, setPickerOpen] = useState(false)
  const [creating, setCreating] = useState(false)

  // Undefined until the project loads — a reader should not see a create button flash.
  const canWrite = project !== null && project.role !== "reader"

  useEffect(() => {
    void (async () => {
      setLoading(true)
      try {
        const [p, pipes, r] = await Promise.all([
          api.getProject(projectId),
          api.listPipelines(projectId),
          api.listRuns(projectId, { limit: RECENT_RUNS }),
        ])
        setProject(p)
        setPipelines(pipes)
        setRuns(r.items)
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load")
      } finally {
        setLoading(false)
      }
    })()
  }, [projectId])

  const createFromTemplate = async (tpl: PipelineTemplate) => {
    setCreating(true)
    try {
      const def = structuredClone(tpl.definition)
      const pipe = await api.createPipeline(projectId, def.name, def)
      setPickerOpen(false)
      await router.navigate({
        to: "/p/$projectId/pipelines/$pipelineId",
        params: { projectId, pipelineId: pipe.id },
      })
    } catch (e) {
      setError(e instanceof Error ? e.message : "Create failed")
    } finally {
      setCreating(false)
    }
  }

  const isShowcase = project?.slug === "showcase"
  // Only the newest RECENT_RUNS runs are loaded here, so a pipeline missing from them
  // may still have history — say "no recent run", never "never run".
  const lastRunFor = (pipelineId: string) =>
    runs.find((r) => r.pipeline_id === pipelineId)

  return (
    <AppShell projectId={projectId} projectName={project?.name}>
      <header className="flex shrink-0 flex-wrap items-center justify-between gap-3 border-border/70 border-b px-8 py-5">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h1 className="truncate font-semibold text-xl tracking-tight">
              {project?.name ?? "Project"}
            </h1>
            {isShowcase ? (
              <Badge className="bg-sky-500/20 text-sky-300 hover:bg-sky-500/20">
                seeded examples
              </Badge>
            ) : null}
            {project?.role ? (
              <span className="text-muted-foreground text-xs">
                you are {project.role}
              </span>
            ) : null}
          </div>
          <p className="text-muted-foreground text-sm">
            {isShowcase
              ? "Open a pipeline to explore diamond, fan-out, release, and retry canvases."
              : "Pipelines and recent runs"}
          </p>
        </div>
        <div className="flex shrink-0 gap-2">
          <Button
            variant="outline"
            render={<Link to="/p/$projectId/settings" params={{ projectId }} />}
          >
            <Sliders className="size-4" />
            Settings
          </Button>
          {canWrite ? (
            <Button onClick={() => setPickerOpen(true)}>
              <Plus className="size-4" />
              New pipeline
            </Button>
          ) : null}
        </div>
      </header>

      <div className="grid min-h-0 flex-1 gap-8 overflow-y-auto px-8 py-6 lg:grid-cols-[1.3fr_1fr]">
        {error ? (
          <p className="text-destructive text-sm lg:col-span-2">{error}</p>
        ) : null}

        <section className="min-w-0">
          <h2 className="mb-3 font-medium text-muted-foreground text-sm">
            Pipelines
          </h2>
          {loading ? (
            <ul className="space-y-2">
              {[0, 1, 2].map((i) => (
                <li
                  key={i}
                  className="h-16 animate-pulse rounded-lg border border-border/50 bg-muted/20"
                />
              ))}
            </ul>
          ) : pipelines.length === 0 ? (
            <div className="rounded-xl border border-border/60 border-dashed px-5 py-8 text-center">
              <p className="text-muted-foreground text-sm">No pipelines yet.</p>
              {canWrite ? (
                <Button
                  className="mt-3"
                  variant="outline"
                  onClick={() => setPickerOpen(true)}
                >
                  <Plus className="size-4" />
                  Start from a template
                </Button>
              ) : null}
            </div>
          ) : (
            <ul className="space-y-2">
              {pipelines.map((p) => {
                const last = lastRunFor(p.id)
                return (
                  <li key={p.id}>
                    <Link
                      to="/p/$projectId/pipelines/$pipelineId"
                      params={{ projectId, pipelineId: p.id }}
                      className="flex items-center justify-between gap-3 rounded-lg border border-border/60 px-4 py-3 transition hover:border-sky-500/40 hover:bg-card/60"
                    >
                      <div className="min-w-0">
                        <div className="flex items-center gap-2 font-medium">
                          <GitBranch className="size-3.5 shrink-0 text-sky-400/80" />
                          <span className="truncate">{p.name}</span>
                        </div>
                        <div className="mt-0.5 truncate text-muted-foreground text-xs">
                          {pipelineBlurb(p)}
                        </div>
                      </div>
                      {last ? (
                        <span className="flex shrink-0 items-center gap-1.5 text-muted-foreground text-xs">
                          <span
                            className="size-2 rounded-full"
                            style={{ background: statusColor(last.status) }}
                            title={last.status}
                          />
                          {relative(last.created_at)}
                        </span>
                      ) : (
                        <span className="shrink-0 text-muted-foreground text-xs">
                          no recent run
                        </span>
                      )}
                    </Link>
                  </li>
                )
              })}
            </ul>
          )}
        </section>

        <section className="min-w-0">
          <div className="mb-3 flex items-center justify-between">
            <h2 className="font-medium text-muted-foreground text-sm">
              Recent runs
            </h2>
            <Link
              to="/p/$projectId/runs"
              params={{ projectId }}
              className="inline-flex items-center gap-1 text-muted-foreground text-xs hover:text-foreground"
            >
              All runs
              <ArrowRight className="size-3" />
            </Link>
          </div>
          <ul className="space-y-1.5">
            {runs.map((r) => (
              <li key={r.id}>
                <Link
                  to="/p/$projectId/runs/$runId"
                  params={{ projectId, runId: r.id }}
                  className="flex items-center justify-between gap-2 rounded-lg border border-border/60 px-3 py-2.5 transition hover:border-sky-500/40 hover:bg-card/60"
                >
                  <span className="flex min-w-0 items-center gap-2">
                    <span
                      className="size-2 shrink-0 rounded-full"
                      style={{ background: statusColor(r.status) }}
                    />
                    <span className="truncate font-mono text-xs">
                      {r.id.slice(0, 8)}
                    </span>
                    <Badge variant="secondary" className="shrink-0">
                      {r.trigger}
                    </Badge>
                  </span>
                  <span className="shrink-0 text-muted-foreground text-xs">
                    {relative(r.created_at)}
                  </span>
                </Link>
              </li>
            ))}
            {!loading && runs.length === 0 ? (
              <p className="text-muted-foreground text-sm">No runs yet.</p>
            ) : null}
          </ul>
        </section>
      </div>

      <Dialog open={pickerOpen} onOpenChange={setPickerOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>New pipeline</DialogTitle>
            <DialogDescription>
              Start from a template — you can edit the canvas after create.
            </DialogDescription>
          </DialogHeader>
          <ul className="mt-2 max-h-[60vh] space-y-2 overflow-y-auto">
            {PIPELINE_TEMPLATES.map((t) => (
              <li key={t.id}>
                <button
                  type="button"
                  disabled={creating}
                  onClick={() => void createFromTemplate(t)}
                  className="flex w-full flex-col rounded-lg border border-border/60 px-4 py-3 text-left transition hover:border-sky-500/50 hover:bg-muted/40 disabled:opacity-50"
                >
                  <span className="font-medium text-sm">{t.name}</span>
                  <span className="mt-0.5 text-muted-foreground text-xs">
                    {t.blurb}
                  </span>
                  <span className="mt-1 font-mono text-[10px] text-muted-foreground/80">
                    {t.definition.steps.length} steps
                    {t.definition.workspace?.repo ? " · git workspace" : ""}
                    {t.definition.steps.some(
                      (s) => (s.artifacts?.length ?? 0) > 0
                    )
                      ? " · artifacts"
                      : ""}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </DialogContent>
      </Dialog>
    </AppShell>
  )
}
