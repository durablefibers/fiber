import { createFileRoute, Link, useRouter } from "@tanstack/react-router"
import { GitBranch, Plus } from "lucide-react"
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
import { Input } from "@/components/ui/input"
import {
  api,
  type Pipeline,
  type Project,
  type ProjectMember,
  type Run,
  type SecretMeta,
  statusColor,
} from "@/lib/api"
import { PIPELINE_TEMPLATES, type PipelineTemplate } from "@/lib/templates"

export const Route = createFileRoute("/p/$projectId/")({
  component: ProjectPage,
})

function pipelineBlurb(p: Pipeline): string {
  const n = p.definition.steps?.length ?? 0
  const bits = [`${n} steps`]
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
  return bits.join(" · ")
}

function ProjectPage() {
  const { projectId } = Route.useParams()
  const router = useRouter()
  const [project, setProject] = useState<Project | null>(null)
  const [pipelines, setPipelines] = useState<Pipeline[]>([])
  const [runs, setRuns] = useState<Run[]>([])
  const [error, setError] = useState<string | null>(null)
  const [webhookSecret, setWebhookSecret] = useState("")
  const [webhookMsg, setWebhookMsg] = useState<string | null>(null)
  const [secrets, setSecrets] = useState<SecretMeta[]>([])
  const [secretKey, setSecretKey] = useState("")
  const [secretValue, setSecretValue] = useState("")
  const [secretMsg, setSecretMsg] = useState<string | null>(null)
  const [members, setMembers] = useState<ProjectMember[]>([])
  const [memberUser, setMemberUser] = useState("")
  const [memberRole, setMemberRole] = useState("writer")
  const [memberPassword, setMemberPassword] = useState("")
  const [memberMsg, setMemberMsg] = useState<string | null>(null)
  const [pickerOpen, setPickerOpen] = useState(false)
  const [creating, setCreating] = useState(false)

  const canAdmin = project?.role === "admin" || project?.role === "owner"

  useEffect(() => {
    void (async () => {
      try {
        const [p, pipes, r] = await Promise.all([
          api.getProject(projectId),
          api.listPipelines(projectId),
          api.listRuns(projectId),
        ])
        setProject(p)
        setPipelines(pipes)
        setRuns(r)
        const admin = p.role === "admin" || p.role === "owner"
        if (admin) {
          const [secs, mems] = await Promise.all([
            api.listSecrets(projectId),
            api.listMembers(projectId),
          ])
          setSecrets(secs)
          setMembers(mems)
        } else {
          setSecrets([])
          try {
            setMembers(await api.listMembers(projectId))
          } catch {
            setMembers([])
          }
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load")
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

  const saveWebhook = async () => {
    if (!webhookSecret.trim()) {
      setWebhookMsg("Webhook secret must not be empty.")
      return
    }
    try {
      await api.setGithubSecret(projectId, webhookSecret.trim())
      setWebhookMsg("Webhook secret saved.")
      setWebhookSecret("")
    } catch (e) {
      setWebhookMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  const saveSecret = async () => {
    if (!secretKey.trim()) return
    try {
      await api.upsertSecret(projectId, secretKey.trim(), secretValue)
      setSecrets(await api.listSecrets(projectId))
      setSecretKey("")
      setSecretValue("")
      setSecretMsg("Secret saved.")
    } catch (e) {
      setSecretMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  const removeSecret = async (key: string) => {
    try {
      await api.deleteSecret(projectId, key)
      setSecrets(await api.listSecrets(projectId))
    } catch (e) {
      setSecretMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  const addMember = async () => {
    if (!memberUser.trim()) return
    try {
      await api.addMember(
        projectId,
        memberUser.trim(),
        memberRole,
        memberPassword || undefined
      )
      setMembers(await api.listMembers(projectId))
      setMemberUser("")
      setMemberPassword("")
      setMemberMsg("Member added.")
    } catch (e) {
      setMemberMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  const changeMemberRole = async (userId: string, role: string) => {
    try {
      await api.updateMember(projectId, userId, role)
      setMembers(await api.listMembers(projectId))
    } catch (e) {
      setMemberMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  const removeMember = async (userId: string) => {
    try {
      await api.removeMember(projectId, userId)
      setMembers(await api.listMembers(projectId))
    } catch (e) {
      setMemberMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  const isShowcase = project?.slug === "showcase"

  return (
    <AppShell projectId={projectId} projectName={project?.name}>
      <header className="flex items-center justify-between border-border/70 border-b px-8 py-5">
        <div>
          <div className="flex items-center gap-2">
            <h1 className="font-semibold text-xl tracking-tight">
              {project?.name ?? "Project"}
            </h1>
            {isShowcase ? (
              <Badge className="bg-sky-500/20 text-sky-300 hover:bg-sky-500/20">
                seeded examples
              </Badge>
            ) : null}
          </div>
          <p className="text-muted-foreground text-sm">
            {isShowcase
              ? "Open a pipeline to explore diamond, fan-out, release, and retry canvases."
              : "Pipelines and recent runs"}
          </p>
        </div>
        <Button onClick={() => setPickerOpen(true)}>
          <Plus className="size-4" />
          New pipeline
        </Button>
      </header>
      <div className="grid flex-1 gap-8 px-8 py-6 lg:grid-cols-[1.2fr_1fr]">
        {error ? (
          <p className="text-destructive text-sm lg:col-span-2">{error}</p>
        ) : null}
        <section>
          <h2 className="mb-3 font-medium text-muted-foreground text-sm">
            Pipelines
          </h2>
          <ul className="space-y-2">
            {pipelines.map((p) => (
              <li key={p.id}>
                <Link
                  to="/p/$projectId/pipelines/$pipelineId"
                  params={{ projectId, pipelineId: p.id }}
                  className="flex items-center justify-between rounded-lg border border-border/60 px-4 py-3 transition hover:border-sky-500/40"
                >
                  <div className="min-w-0">
                    <div className="flex items-center gap-2 font-medium">
                      <GitBranch className="size-3.5 shrink-0 text-sky-400/80" />
                      {p.name}
                    </div>
                    <div className="mt-0.5 truncate text-muted-foreground text-xs">
                      {pipelineBlurb(p)}
                    </div>
                  </div>
                </Link>
              </li>
            ))}
            {pipelines.length === 0 ? (
              <p className="text-muted-foreground text-sm">
                No pipelines yet. Pick a template to start on the canvas.
              </p>
            ) : null}
          </ul>
          <div className="mt-8 rounded-lg border border-border/60 p-4">
            <h2 className="font-medium text-sm">Members</h2>
            <p className="mt-1 text-muted-foreground text-xs">
              Roles: reader &lt; writer &lt; admin &lt; owner
              {project?.role ? ` · you are ${project.role}` : ""}
            </p>
            {canAdmin ? (
              <div className="mt-3 flex flex-wrap gap-2">
                <Input
                  className="max-w-[140px]"
                  placeholder="username"
                  value={memberUser}
                  onChange={(e) => setMemberUser(e.target.value)}
                />
                <select
                  className="h-9 rounded-md border border-input bg-transparent px-2 text-sm"
                  value={memberRole}
                  onChange={(e) => setMemberRole(e.target.value)}
                >
                  <option value="reader">reader</option>
                  <option value="writer">writer</option>
                  <option value="admin">admin</option>
                  {project?.role === "owner" ? (
                    <option value="owner">owner</option>
                  ) : null}
                </select>
                <Input
                  className="max-w-[140px]"
                  type="password"
                  placeholder="password (new user)"
                  value={memberPassword}
                  onChange={(e) => setMemberPassword(e.target.value)}
                />
                <Button variant="outline" onClick={() => void addMember()}>
                  Add
                </Button>
              </div>
            ) : null}
            {memberMsg ? (
              <p className="mt-2 text-muted-foreground text-xs">{memberMsg}</p>
            ) : null}
            <ul className="mt-3 space-y-1">
              {members.map((m) => (
                <li
                  key={m.user_id}
                  className="flex items-center justify-between gap-2 rounded-md px-2 py-1 text-sm hover:bg-muted/50"
                >
                  <span>
                    <span className="font-medium">{m.username}</span>
                    <span className="ml-2 text-muted-foreground text-xs">
                      {m.role}
                    </span>
                  </span>
                  {canAdmin ? (
                    <span className="flex items-center gap-1">
                      <select
                        className="h-7 rounded border border-input bg-transparent px-1 text-xs"
                        value={m.role}
                        onChange={(e) =>
                          void changeMemberRole(m.user_id, e.target.value)
                        }
                      >
                        <option value="reader">reader</option>
                        <option value="writer">writer</option>
                        <option value="admin">admin</option>
                        {project?.role === "owner" ? (
                          <option value="owner">owner</option>
                        ) : null}
                      </select>
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => void removeMember(m.user_id)}
                      >
                        Remove
                      </Button>
                    </span>
                  ) : null}
                </li>
              ))}
            </ul>
          </div>
          {canAdmin ? (
            <>
              <div className="mt-8 rounded-lg border border-border/60 p-4">
                <h2 className="font-medium text-sm">Secrets</h2>
                <p className="mt-1 text-muted-foreground text-xs">
                  Injected as env vars into every step on this project.
                </p>
                <div className="mt-3 flex flex-wrap gap-2">
                  <Input
                    className="max-w-[140px]"
                    placeholder="KEY"
                    value={secretKey}
                    onChange={(e) => setSecretKey(e.target.value)}
                  />
                  <Input
                    className="max-w-[180px]"
                    type="password"
                    placeholder="value"
                    value={secretValue}
                    onChange={(e) => setSecretValue(e.target.value)}
                  />
                  <Button variant="outline" onClick={() => void saveSecret()}>
                    Save
                  </Button>
                </div>
                {secretMsg ? (
                  <p className="mt-2 text-muted-foreground text-xs">
                    {secretMsg}
                  </p>
                ) : null}
                <ul className="mt-3 space-y-1">
                  {secrets.map((s) => (
                    <li
                      key={s.id}
                      className="flex items-center justify-between rounded-md px-2 py-1 text-sm hover:bg-muted/50"
                    >
                      <code className="text-xs">{s.key}</code>
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => void removeSecret(s.key)}
                      >
                        Delete
                      </Button>
                    </li>
                  ))}
                </ul>
              </div>
              <div className="mt-8 rounded-lg border border-border/60 p-4">
                <h2 className="font-medium text-sm">GitHub webhook</h2>
                <p className="mt-1 text-muted-foreground text-xs">
                  For PR path filters, also add a{" "}
                  <code className="text-[11px]">GITHUB_TOKEN</code> project
                  secret (contents:read).
                </p>
                <code className="mt-2 block break-all text-[11px] text-muted-foreground">
                  {api.apiUrl}/api/projects/{projectId}/webhooks/github
                </code>
                <div className="mt-3 flex gap-2">
                  <Input
                    type="password"
                    placeholder="Webhook secret"
                    value={webhookSecret}
                    onChange={(e) => setWebhookSecret(e.target.value)}
                  />
                  <Button variant="outline" onClick={() => void saveWebhook()}>
                    Save
                  </Button>
                </div>
                {webhookMsg ? (
                  <p className="mt-2 text-muted-foreground text-xs">
                    {webhookMsg}
                  </p>
                ) : null}
              </div>
            </>
          ) : null}
        </section>
        <section>
          <h2 className="mb-3 font-medium text-muted-foreground text-sm">
            Recent runs
          </h2>
          <ul className="space-y-2">
            {runs.map((r) => (
              <li key={r.id}>
                <Link
                  to="/p/$projectId/runs/$runId"
                  params={{ projectId, runId: r.id }}
                  className="flex items-center justify-between rounded-lg border border-border/60 px-4 py-3 transition hover:border-sky-500/40"
                >
                  <div className="flex items-center gap-2">
                    <span
                      className="size-2 rounded-full"
                      style={{ background: statusColor(r.status) }}
                    />
                    <span className="font-mono text-xs">
                      {r.id.slice(0, 8)}
                    </span>
                    <Badge variant="secondary">{r.trigger}</Badge>
                  </div>
                  <span className="text-muted-foreground text-xs capitalize">
                    {r.status}
                  </span>
                </Link>
              </li>
            ))}
            {runs.length === 0 ? (
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
