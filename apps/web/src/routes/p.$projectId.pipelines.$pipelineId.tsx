import { createFileRoute, Link, useRouter } from "@tanstack/react-router"
import {
  ChevronRight,
  Clock,
  FileDown,
  FileUp,
  GitBranch,
  Play,
  Save,
  Trash2,
} from "lucide-react"
import { useEffect, useId, useMemo, useState } from "react"
import { toast } from "sonner"
import { AppShell } from "@/components/app-shell"
import { DagCanvas } from "@/components/dag-canvas"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import {
  api,
  definitionToYaml,
  type Pipeline,
  type PipelineDefinition,
  type Project,
} from "@/lib/api"
import { cn } from "@/lib/utils"

export const Route = createFileRoute("/p/$projectId/pipelines/$pipelineId")({
  validateSearch: (search: Record<string, unknown>): { step?: string } => ({
    step:
      typeof search.step === "string" && search.step ? search.step : undefined,
  }),
  component: PipelineEditorPage,
})

function formatInterval(minutes: number): string {
  if (minutes >= 1440 && minutes % 1440 === 0) {
    const d = minutes / 1440
    return d === 1 ? "daily" : `every ${d}d`
  }
  if (minutes >= 60 && minutes % 60 === 0) {
    const h = minutes / 60
    return h === 1 ? "hourly" : `every ${h}h`
  }
  return `every ${minutes}m`
}

function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: React.ReactNode
}) {
  const id = useId()
  const control = Array.isArray(children) ? children[0] : children
  const slotted =
    control && typeof control === "object" && "props" in control
      ? {
          ...(control as React.ReactElement),
          props: {
            ...(control as React.ReactElement<{ id?: string }>).props,
            id: (control as React.ReactElement<{ id?: string }>).props.id ?? id,
          },
        }
      : children

  return (
    <div className="space-y-1.5">
      <div className="flex items-baseline justify-between gap-2">
        <label
          htmlFor={id}
          className="font-medium text-[11px] text-muted-foreground uppercase tracking-wide"
        >
          {label}
        </label>
        {hint ? (
          <span className="text-[10px] text-muted-foreground/70">{hint}</span>
        ) : null}
      </div>
      {slotted}
    </div>
  )
}

function PipelineEditorPage() {
  const { projectId, pipelineId } = Route.useParams()
  const { step: stepSearch } = Route.useSearch()
  const navigate = Route.useNavigate()
  const router = useRouter()
  const [project, setProject] = useState<Project | null>(null)
  const [, setPipeline] = useState<Pipeline | null>(null)
  const [definition, setDefinition] = useState<PipelineDefinition | null>(null)
  const [baseline, setBaseline] = useState<string>("")
  const [selectedId, setSelectedId] = useState<string | null>(
    stepSearch ?? null
  )
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [running, setRunning] = useState(false)
  const [yamlOpen, setYamlOpen] = useState(false)
  const [yamlText, setYamlText] = useState("")
  const [inspector, setInspector] = useState<"pipeline" | "step">("step")
  const [showAdvanced, setShowAdvanced] = useState(false)

  useEffect(() => {
    void (async () => {
      try {
        const [p, pipe] = await Promise.all([
          api.getProject(projectId),
          api.getPipeline(pipelineId),
        ])
        setProject(p)
        setPipeline(pipe)
        setDefinition(pipe.definition)
        setBaseline(JSON.stringify(pipe.definition))
        const fromSearch = stepSearch
          ? pipe.definition.steps.find((s) => s.id === stepSearch)?.id
          : undefined
        const nextId = fromSearch ?? pipe.definition.steps[0]?.id ?? null
        setSelectedId(nextId)
        setInspector(nextId ? "step" : "pipeline")
        if (fromSearch && stepSearch !== fromSearch) {
          void navigate({
            search: (prev) => ({ ...prev, step: fromSearch }),
            replace: true,
          })
        } else if (!stepSearch && nextId) {
          void navigate({
            search: (prev) => ({ ...prev, step: nextId }),
            replace: true,
          })
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load")
      }
    })()
  }, [projectId, pipelineId])

  const selected = definition?.steps.find((s) => s.id === selectedId)
  const dirty = useMemo(
    () => (definition ? JSON.stringify(definition) !== baseline : false),
    [definition, baseline]
  )

  useEffect(() => {
    if (!selected) {
      setShowAdvanced(false)
      return
    }
    const advanced =
      Boolean(selected.image) ||
      (selected.retries ?? 0) > 0 ||
      (selected.artifacts?.length ?? 0) > 0 ||
      (selected.labels?.length ?? 0) > 0 ||
      Boolean(selected.if) ||
      Boolean(selected.matrix)
    setShowAdvanced(advanced)
  }, [selected?.id])

  const selectStep = (id: string | null) => {
    setSelectedId(id)
    if (id) setInspector("step")
    void navigate({
      search: (prev) => ({ ...prev, step: id ?? undefined }),
      replace: true,
    })
  }

  const save = async () => {
    if (!definition) return
    setSaving(true)
    setError(null)
    try {
      const updated = await api.updatePipeline(
        pipelineId,
        definition,
        definition.name
      )
      setPipeline(updated)
      setDefinition(updated.definition)
      setBaseline(JSON.stringify(updated.definition))
      toast.success("Pipeline saved")
    } catch (e) {
      setError(e instanceof Error ? e.message : "Save failed")
    } finally {
      setSaving(false)
    }
  }

  const run = async () => {
    setRunning(true)
    setError(null)
    try {
      if (definition && dirty) {
        await save()
      }
      const res = await api.startRun(pipelineId)
      toast.success("Run started")
      await router.navigate({
        to: "/p/$projectId/runs/$runId",
        params: { projectId, runId: res.run.id },
      })
    } catch (e) {
      setError(e instanceof Error ? e.message : "Run failed")
    } finally {
      setRunning(false)
    }
  }

  const updateSelected = (patch: Partial<NonNullable<typeof selected>>) => {
    if (!definition || !selected) return
    setDefinition({
      ...definition,
      steps: definition.steps.map((s) =>
        s.id === selected.id ? { ...s, ...patch } : s
      ),
    })
  }

  const toggleNeed = (needId: string) => {
    if (!selected) return
    const current = new Set(selected.needs ?? [])
    if (current.has(needId)) current.delete(needId)
    else current.add(needId)
    updateSelected({ needs: [...current] })
  }

  const deleteSelected = () => {
    if (!definition || !selected) return
    const id = selected.id
    const steps = definition.steps
      .filter((s) => s.id !== id)
      .map((s) => ({
        ...s,
        needs: (s.needs ?? []).filter((n) => n !== id),
      }))
    setDefinition({ ...definition, steps })
    selectStep(steps[0]?.id ?? null)
  }

  const openExport = () => {
    if (!definition) return
    setYamlText(definitionToYaml(definition))
    setYamlOpen(true)
  }

  const applyImport = async () => {
    try {
      const def = await api.parseYaml(yamlText)
      setDefinition(def)
      selectStep(def.steps[0]?.id ?? null)
      setYamlOpen(false)
      toast.success("YAML applied — save to persist")
    } catch (e) {
      setError(e instanceof Error ? e.message : "YAML import failed")
    }
  }

  const discard = () => {
    if (!baseline) return
    try {
      const def = JSON.parse(baseline) as PipelineDefinition
      setDefinition(def)
      selectStep(def.steps[0]?.id ?? null)
      toast.message("Changes discarded")
    } catch {
      setError("Could not restore saved definition")
    }
  }

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null
      const typing =
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.isContentEditable)
      const meta = e.metaKey || e.ctrlKey
      if (meta && e.key.toLowerCase() === "s") {
        e.preventDefault()
        if (dirty && !saving) void save()
        return
      }
      if (e.key === "Escape" && !typing) {
        setInspector("pipeline")
        selectStep(null)
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  })

  useEffect(() => {
    if (!dirty) return
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      e.returnValue = ""
    }
    window.addEventListener("beforeunload", onBeforeUnload)
    return () => window.removeEventListener("beforeunload", onBeforeUnload)
  }, [dirty])

  const intervalMinutes = definition?.on?.interval_minutes
  const intervalLabel =
    intervalMinutes && intervalMinutes > 0
      ? formatInterval(intervalMinutes)
      : null
  const cronLabel = definition?.on?.cron?.trim() || null

  return (
    <AppShell projectId={projectId} projectName={project?.name}>
      <header className="flex shrink-0 items-center justify-between gap-4 border-border/70 border-b px-5 py-3">
        <div className="min-w-0 flex-1">
          <div className="mb-1 flex items-center gap-1 text-[11px] text-muted-foreground">
            <Link
              to="/p/$projectId"
              params={{ projectId }}
              className="hover:text-foreground"
            >
              {project?.name ?? "Project"}
            </Link>
            <ChevronRight className="size-3 opacity-50" />
            <span className="truncate text-foreground/80">
              {definition?.name ?? "Pipeline"}
            </span>
            {dirty ? (
              <Badge
                variant="secondary"
                className="ml-1 h-5 bg-amber-500/15 text-[10px] text-amber-200"
              >
                unsaved
              </Badge>
            ) : null}
          </div>
          <div className="flex items-center gap-2">
            <Input
              className="h-8 max-w-xs border-transparent bg-transparent px-0 font-semibold text-base shadow-none focus-visible:ring-0"
              value={definition?.name ?? ""}
              onChange={(e) =>
                definition &&
                setDefinition({ ...definition, name: e.target.value })
              }
            />
            <span className="hidden items-center gap-1 text-muted-foreground text-xs sm:inline-flex">
              <GitBranch className="size-3.5" />
              {definition?.steps.length ?? 0} steps
            </span>
            {intervalLabel ? (
              <span className="hidden items-center gap-1 text-muted-foreground text-xs md:inline-flex">
                <Clock className="size-3.5" />
                {intervalLabel}
              </span>
            ) : null}
            {cronLabel ? (
              <span
                className="hidden max-w-[180px] truncate font-mono text-[11px] text-muted-foreground lg:inline"
                title={cronLabel}
              >
                cron {cronLabel}
              </span>
            ) : null}
          </div>
        </div>
        <div className="flex shrink-0 gap-2">
          {dirty ? (
            <Button variant="ghost" size="sm" onClick={discard}>
              Discard
            </Button>
          ) : null}
          <Button variant="outline" size="sm" onClick={openExport}>
            <FileDown className="size-4" />
            Export
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              setYamlText("")
              setYamlOpen(true)
            }}
          >
            <FileUp className="size-4" />
            Import
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => void save()}
            disabled={saving || !dirty}
            title="⌘S / Ctrl+S"
          >
            <Save className="size-4" />
            {saving ? "Saving…" : "Save"}
          </Button>
          <Button size="sm" onClick={() => void run()} disabled={running}>
            <Play className="size-4" />
            {running ? "Starting…" : "Run"}
          </Button>
        </div>
      </header>

      {error ? (
        <p className="shrink-0 border-destructive/30 border-b bg-destructive/10 px-5 py-2 text-destructive text-sm">
          {error}
        </p>
      ) : null}

      {yamlOpen ? (
        <div className="shrink-0 border-border/70 border-b bg-muted/25 px-5 py-4">
          <div className="mb-2 flex items-center justify-between">
            <h3 className="font-medium text-sm">fiber.yml</h3>
            <div className="flex gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => setYamlOpen(false)}
              >
                Close
              </Button>
              <Button size="sm" onClick={() => void applyImport()}>
                Apply YAML
              </Button>
            </div>
          </div>
          <Textarea
            className="min-h-44 font-mono text-xs"
            value={yamlText}
            onChange={(e) => setYamlText(e.target.value)}
            placeholder="Paste fiber.yml here…"
          />
        </div>
      ) : null}

      <div className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[minmax(0,1fr)_360px]">
        <div className="min-h-[420px] p-3 lg:min-h-0 lg:p-4">
          {definition ? (
            <DagCanvas
              definition={definition}
              editable
              selectedStepId={selectedId}
              onSelectStep={selectStep}
              onChange={(d) => setDefinition(d)}
            />
          ) : (
            <div className="flex h-full min-h-[420px] items-center justify-center rounded-2xl border border-border/60 border-dashed text-muted-foreground text-sm">
              Loading pipeline…
            </div>
          )}
        </div>

        <aside className="flex min-h-0 flex-col border-border/70 border-t bg-muted/10 lg:border-t-0 lg:border-l">
          <div className="flex shrink-0 gap-1 border-border/60 border-b p-2">
            {(
              [
                ["pipeline", "Pipeline"],
                ["step", "Step"],
              ] as const
            ).map(([key, label]) => (
              <button
                key={key}
                type="button"
                onClick={() => setInspector(key)}
                className={cn(
                  "flex-1 rounded-md px-3 py-1.5 font-medium text-xs transition",
                  inspector === key
                    ? "bg-sky-500/15 text-sky-300"
                    : "text-muted-foreground hover:bg-muted/60"
                )}
              >
                {label}
              </button>
            ))}
          </div>

          <div className="min-h-0 flex-1 overflow-y-auto p-4">
            {inspector === "pipeline" ? (
              <div className="space-y-5">
                <section className="space-y-3">
                  <h2 className="font-semibold text-muted-foreground text-xs uppercase tracking-wide">
                    Workspace
                  </h2>
                  <Field label="Git repo" hint="optional clone">
                    <Input
                      value={definition?.workspace?.repo ?? ""}
                      onChange={(e) => {
                        if (!definition) return
                        const repo = e.target.value.trim()
                        setDefinition({
                          ...definition,
                          workspace: repo
                            ? {
                                repo,
                                ref: definition.workspace?.ref ?? "main",
                              }
                            : undefined,
                        })
                      }}
                      placeholder="https://github.com/org/repo.git"
                    />
                  </Field>
                  <Field label="Ref">
                    <Input
                      value={definition?.workspace?.ref ?? ""}
                      disabled={!definition?.workspace?.repo}
                      onChange={(e) => {
                        if (!definition?.workspace?.repo) return
                        setDefinition({
                          ...definition,
                          workspace: {
                            ...definition.workspace,
                            ref: e.target.value || "main",
                          },
                        })
                      }}
                      placeholder="main"
                    />
                  </Field>
                </section>

                <section className="space-y-3">
                  <h2 className="font-semibold text-muted-foreground text-xs uppercase tracking-wide">
                    Triggers
                  </h2>
                  <Field label="Push branches" hint="comma-separated">
                    <Input
                      value={(definition?.on?.push?.branches ?? []).join(", ")}
                      onChange={(e) => {
                        if (!definition) return
                        const branches = e.target.value
                          .split(",")
                          .map((x) => x.trim())
                          .filter(Boolean)
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            push: {
                              ...definition.on?.push,
                              branches: branches.length ? branches : undefined,
                            },
                          },
                        })
                      }}
                      placeholder="main, develop"
                    />
                  </Field>
                  <Field label="Push paths" hint="globs · comma-separated">
                    <Input
                      value={(definition?.on?.push?.paths ?? []).join(", ")}
                      onChange={(e) => {
                        if (!definition) return
                        const paths = e.target.value
                          .split(",")
                          .map((x) => x.trim())
                          .filter(Boolean)
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            push: {
                              ...definition.on?.push,
                              paths: paths.length ? paths : undefined,
                            },
                          },
                        })
                      }}
                      placeholder="src/**, Cargo.toml"
                      className="font-mono text-xs"
                    />
                  </Field>
                  <Field
                    label="Push paths-ignore"
                    hint="globs · comma-separated"
                  >
                    <Input
                      value={(definition?.on?.push?.paths_ignore ?? []).join(
                        ", "
                      )}
                      onChange={(e) => {
                        if (!definition) return
                        const paths_ignore = e.target.value
                          .split(",")
                          .map((x) => x.trim())
                          .filter(Boolean)
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            push: {
                              ...definition.on?.push,
                              paths_ignore: paths_ignore.length
                                ? paths_ignore
                                : undefined,
                            },
                          },
                        })
                      }}
                      placeholder="**/*.md"
                      className="font-mono text-xs"
                    />
                  </Field>
                  <Field label="PR base branches" hint="comma-separated">
                    <Input
                      value={(
                        definition?.on?.pull_request?.branches ?? []
                      ).join(", ")}
                      onChange={(e) => {
                        if (!definition) return
                        const branches = e.target.value
                          .split(",")
                          .map((x) => x.trim())
                          .filter(Boolean)
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            pull_request: {
                              ...definition.on?.pull_request,
                              branches: branches.length ? branches : undefined,
                            },
                          },
                        })
                      }}
                      placeholder="main"
                    />
                  </Field>
                  <Field label="PR paths" hint="needs GITHUB_TOKEN">
                    <Input
                      value={(definition?.on?.pull_request?.paths ?? []).join(
                        ", "
                      )}
                      onChange={(e) => {
                        if (!definition) return
                        const paths = e.target.value
                          .split(",")
                          .map((x) => x.trim())
                          .filter(Boolean)
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            pull_request: {
                              ...definition.on?.pull_request,
                              paths: paths.length ? paths : undefined,
                            },
                          },
                        })
                      }}
                      placeholder="src/**"
                      className="font-mono text-xs"
                    />
                  </Field>
                  <Field label="PR paths-ignore" hint="globs">
                    <Input
                      value={(
                        definition?.on?.pull_request?.paths_ignore ?? []
                      ).join(", ")}
                      onChange={(e) => {
                        if (!definition) return
                        const paths_ignore = e.target.value
                          .split(",")
                          .map((x) => x.trim())
                          .filter(Boolean)
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            pull_request: {
                              ...definition.on?.pull_request,
                              paths_ignore: paths_ignore.length
                                ? paths_ignore
                                : undefined,
                            },
                          },
                        })
                      }}
                      placeholder="**/*.md"
                      className="font-mono text-xs"
                    />
                  </Field>
                  <Field
                    label="Interval"
                    hint={
                      intervalMinutes
                        ? formatInterval(intervalMinutes)
                        : "minutes · 0 = off"
                    }
                  >
                    <Input
                      type="number"
                      min={0}
                      value={definition?.on?.interval_minutes ?? ""}
                      onChange={(e) => {
                        if (!definition) return
                        const v = e.target.value
                          ? Number(e.target.value)
                          : undefined
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            interval_minutes: v && v > 0 ? v : undefined,
                          },
                        })
                      }}
                      placeholder="off"
                    />
                  </Field>
                  <Field
                    label="Cron"
                    hint="6 fields w/ seconds · overrides interval"
                  >
                    <Input
                      value={definition?.on?.cron ?? ""}
                      onChange={(e) => {
                        if (!definition) return
                        const v = e.target.value.trim()
                        setDefinition({
                          ...definition,
                          on: {
                            ...definition.on,
                            cron: v || undefined,
                          },
                        })
                      }}
                      placeholder="0 */15 * * * *"
                      className="font-mono text-xs"
                    />
                  </Field>
                </section>

                <p className="text-[11px] text-muted-foreground leading-relaxed">
                  Connect steps on the canvas to set dependencies. Click a node
                  to edit its command in the Step tab.
                </p>
              </div>
            ) : (
              <div className="space-y-4">
                <div className="space-y-1.5">
                  <h2 className="font-semibold text-muted-foreground text-xs uppercase tracking-wide">
                    Steps
                  </h2>
                  <div className="space-y-0.5">
                    {definition?.steps.map((s) => {
                      const needs = s.needs?.length ?? 0
                      return (
                        <button
                          key={s.id}
                          type="button"
                          onClick={() => selectStep(s.id)}
                          className={cn(
                            "flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left transition",
                            selectedId === s.id
                              ? "bg-sky-500/15 text-sky-100"
                              : "hover:bg-muted/70"
                          )}
                        >
                          <span
                            className={cn(
                              "size-1.5 shrink-0 rounded-full",
                              selectedId === s.id
                                ? "bg-sky-400"
                                : "bg-muted-foreground/40"
                            )}
                          />
                          <span className="min-w-0 flex-1">
                            <span className="block truncate font-medium text-sm">
                              {s.name}
                            </span>
                            {s.name !== s.id ? (
                              <span className="block truncate font-mono text-[10px] text-muted-foreground">
                                {s.id}
                              </span>
                            ) : null}
                          </span>
                          <span className="shrink-0 rounded-md bg-black/20 px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
                            {needs === 0 ? "root" : `needs ${needs}`}
                          </span>
                        </button>
                      )
                    })}
                    {!definition?.steps.length ? (
                      <p className="px-1 text-muted-foreground text-xs">
                        No steps yet — use Add step on the canvas.
                      </p>
                    ) : null}
                  </div>
                </div>

                {selected ? (
                  <div className="space-y-3.5 border-border/50 border-t pt-4">
                    <div className="flex items-center justify-between gap-2">
                      <div className="min-w-0">
                        <h3 className="truncate font-medium text-sm">
                          {selected.name}
                        </h3>
                        <p className="font-mono text-[10px] text-muted-foreground">
                          {selected.id}
                        </p>
                      </div>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 shrink-0 text-destructive hover:text-destructive"
                        onClick={deleteSelected}
                      >
                        <Trash2 className="size-3.5" />
                        Delete
                      </Button>
                    </div>

                    <Field label="Name">
                      <Input
                        value={selected.name}
                        onChange={(e) =>
                          updateSelected({ name: e.target.value })
                        }
                      />
                    </Field>

                    <Field label="Run" hint="shell">
                      <Textarea
                        className="min-h-36 font-mono text-xs leading-relaxed"
                        value={selected.run ?? ""}
                        onChange={(e) =>
                          updateSelected({ run: e.target.value })
                        }
                        placeholder='echo "hello"'
                      />
                    </Field>

                    <div className="space-y-2">
                      <div className="flex items-baseline justify-between gap-2">
                        <span className="font-medium text-[11px] text-muted-foreground uppercase tracking-wide">
                          Needs
                        </span>
                        <span className="text-[10px] text-muted-foreground/70">
                          or drag edges
                        </span>
                      </div>
                      <div className="flex flex-wrap gap-1.5">
                        {definition?.steps
                          .filter((s) => s.id !== selected.id)
                          .map((s) => {
                            const on = (selected.needs ?? []).includes(s.id)
                            return (
                              <button
                                key={s.id}
                                type="button"
                                onClick={() => toggleNeed(s.id)}
                                className={cn(
                                  "rounded-md border px-2 py-1 font-medium text-[11px] transition",
                                  on
                                    ? "border-sky-400/40 bg-sky-500/15 text-sky-200"
                                    : "border-border/60 text-muted-foreground hover:border-border hover:text-foreground"
                                )}
                              >
                                {s.name}
                              </button>
                            )
                          })}
                        {(definition?.steps.length ?? 0) <= 1 ? (
                          <span className="text-[11px] text-muted-foreground">
                            Add another step to create dependencies.
                          </span>
                        ) : null}
                      </div>
                    </div>

                    <button
                      type="button"
                      onClick={() => setShowAdvanced((v) => !v)}
                      className="font-medium text-[11px] text-muted-foreground uppercase tracking-wide hover:text-foreground"
                    >
                      {showAdvanced ? "Hide advanced" : "Show advanced"}
                    </button>

                    {showAdvanced ? (
                      <div className="space-y-3">
                        <Field label="If" hint="success() · always() · never()">
                          <Input
                            value={selected.if ?? ""}
                            onChange={(e) =>
                              updateSelected({
                                if: e.target.value.trim() || undefined,
                              })
                            }
                            placeholder="matrix.os == 'linux'"
                            className="font-mono text-xs"
                          />
                        </Field>
                        <Field
                          label="Matrix"
                          hint='JSON object · {"os":["linux","macos"]}'
                        >
                          <Input
                            value={
                              selected.matrix
                                ? JSON.stringify(selected.matrix)
                                : ""
                            }
                            onChange={(e) => {
                              const raw = e.target.value.trim()
                              if (!raw) {
                                updateSelected({ matrix: undefined })
                                return
                              }
                              try {
                                const parsed = JSON.parse(raw) as Record<
                                  string,
                                  string[]
                                >
                                updateSelected({ matrix: parsed })
                              } catch {
                                /* keep typing */
                              }
                            }}
                            placeholder='{"os":["linux","macos"]}'
                            className="font-mono text-xs"
                          />
                        </Field>
                        <Field label="Image" hint="optional docker">
                          <Input
                            value={selected.image ?? ""}
                            onChange={(e) =>
                              updateSelected({
                                image: e.target.value || undefined,
                              })
                            }
                            placeholder="rust:1.85"
                          />
                        </Field>
                        <Field label="Labels">
                          <Input
                            value={(selected.labels ?? []).join(", ")}
                            onChange={(e) =>
                              updateSelected({
                                labels: e.target.value
                                  .split(",")
                                  .map((x) => x.trim())
                                  .filter(Boolean),
                              })
                            }
                            placeholder="os=linux"
                          />
                        </Field>
                        <div className="grid grid-cols-2 gap-3">
                          <Field label="Retries">
                            <Input
                              type="number"
                              min={0}
                              value={selected.retries ?? 0}
                              onChange={(e) =>
                                updateSelected({
                                  retries: Number(e.target.value) || 0,
                                })
                              }
                            />
                          </Field>
                          <Field label="Artifacts">
                            <Input
                              value={(selected.artifacts ?? []).join(", ")}
                              onChange={(e) =>
                                updateSelected({
                                  artifacts: e.target.value
                                    .split(",")
                                    .map((x) => x.trim())
                                    .filter(Boolean),
                                })
                              }
                              placeholder="out/*.tgz"
                            />
                          </Field>
                        </div>
                      </div>
                    ) : null}
                  </div>
                ) : (
                  <p className="border-border/50 border-t pt-4 text-muted-foreground text-sm">
                    Select a step on the canvas or in the list.
                  </p>
                )}
              </div>
            )}
          </div>
        </aside>
      </div>
    </AppShell>
  )
}
