import { createFileRoute } from "@tanstack/react-router"
import { KeyRound, Users, Webhook } from "lucide-react"
import { useEffect, useState } from "react"
import { toast } from "sonner"
import { AppShell } from "@/components/app-shell"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  api,
  type Project,
  type ProjectMember,
  type SecretMeta,
} from "@/lib/api"

export const Route = createFileRoute("/p/$projectId/settings")({
  component: ProjectSettingsPage,
})

const ROLES = ["reader", "writer", "admin"] as const

function Card({
  title,
  description,
  icon: Icon,
  children,
}: {
  title: string
  description: React.ReactNode
  icon: React.ComponentType<{ className?: string }>
  children: React.ReactNode
}) {
  return (
    <section className="rounded-xl border border-border/60 bg-card/30 p-5">
      <div className="flex items-start gap-3">
        <span className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-lg bg-sky-500/10 text-sky-400">
          <Icon className="size-4" />
        </span>
        <div className="min-w-0">
          <h2 className="font-medium text-sm">{title}</h2>
          <p className="mt-0.5 text-muted-foreground text-xs">{description}</p>
        </div>
      </div>
      <div className="mt-4">{children}</div>
    </section>
  )
}

function ProjectSettingsPage() {
  const { projectId } = Route.useParams()
  const [project, setProject] = useState<Project | null>(null)
  const [members, setMembers] = useState<ProjectMember[]>([])
  const [secrets, setSecrets] = useState<SecretMeta[]>([])
  const [memberUser, setMemberUser] = useState("")
  const [memberRole, setMemberRole] = useState("writer")
  const [memberPassword, setMemberPassword] = useState("")
  const [secretKey, setSecretKey] = useState("")
  const [secretValue, setSecretValue] = useState("")
  const [webhookSecret, setWebhookSecret] = useState("")
  const [error, setError] = useState<string | null>(null)

  const canAdmin = project?.role === "admin" || project?.role === "owner"
  const isOwner = project?.role === "owner"
  const roles = isOwner ? [...ROLES, "owner"] : ROLES

  useEffect(() => {
    void (async () => {
      try {
        const p = await api.getProject(projectId)
        setProject(p)
        try {
          setMembers(await api.listMembers(projectId))
        } catch {
          setMembers([])
        }
        if (p.role === "admin" || p.role === "owner") {
          setSecrets(await api.listSecrets(projectId))
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load")
      }
    })()
  }, [projectId])

  const fail = (e: unknown, fallback: string) =>
    toast.error(e instanceof Error ? e.message : fallback)

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
      toast.success("Member added.")
    } catch (e) {
      fail(e, "Could not add member")
    }
  }

  const changeRole = async (userId: string, role: string) => {
    try {
      await api.updateMember(projectId, userId, role)
      setMembers(await api.listMembers(projectId))
    } catch (e) {
      fail(e, "Could not change role")
    }
  }

  const removeMember = async (userId: string) => {
    try {
      await api.removeMember(projectId, userId)
      setMembers(await api.listMembers(projectId))
    } catch (e) {
      fail(e, "Could not remove member")
    }
  }

  const saveSecret = async () => {
    if (!secretKey.trim()) return
    try {
      await api.upsertSecret(projectId, secretKey.trim(), secretValue)
      setSecrets(await api.listSecrets(projectId))
      setSecretKey("")
      setSecretValue("")
      toast.success("Secret saved.")
    } catch (e) {
      fail(e, "Could not save secret")
    }
  }

  const removeSecret = async (key: string) => {
    try {
      await api.deleteSecret(projectId, key)
      setSecrets(await api.listSecrets(projectId))
    } catch (e) {
      fail(e, "Could not delete secret")
    }
  }

  const saveWebhook = async () => {
    if (!webhookSecret.trim()) {
      toast.error("Webhook secret must not be empty.")
      return
    }
    try {
      await api.setGithubSecret(projectId, webhookSecret.trim())
      setWebhookSecret("")
      toast.success("Webhook secret saved.")
    } catch (e) {
      fail(e, "Could not save webhook secret")
    }
  }

  return (
    <AppShell projectId={projectId} projectName={project?.name}>
      <header className="shrink-0 border-border/70 border-b px-8 py-5">
        <h1 className="font-semibold text-xl tracking-tight">
          Project settings
        </h1>
        <p className="text-muted-foreground text-sm">
          Members, secrets, and the GitHub webhook for{" "}
          {project?.name ?? "this project"}.
        </p>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">
        {error ? (
          <p className="mb-4 text-destructive text-sm">{error}</p>
        ) : null}
        <div className="grid max-w-5xl gap-5 lg:grid-cols-2">
          <Card
            title="Members"
            description={
              <>
                reader &lt; writer &lt; admin &lt; owner
                {project?.role ? ` · you are ${project.role}` : ""}
              </>
            }
            icon={Users}
          >
            {canAdmin ? (
              <div className="flex flex-wrap gap-2">
                <Input
                  className="max-w-[150px]"
                  placeholder="username"
                  value={memberUser}
                  onChange={(e) => setMemberUser(e.target.value)}
                />
                <select
                  aria-label="Role for the new member"
                  className="h-9 rounded-md border border-input bg-transparent px-2 text-sm"
                  value={memberRole}
                  onChange={(e) => setMemberRole(e.target.value)}
                >
                  {roles.map((r) => (
                    <option key={r} value={r}>
                      {r}
                    </option>
                  ))}
                </select>
                <Input
                  className="max-w-[170px]"
                  type="password"
                  placeholder="password (new user)"
                  value={memberPassword}
                  onChange={(e) => setMemberPassword(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && void addMember()}
                />
                <Button variant="outline" onClick={() => void addMember()}>
                  Add
                </Button>
              </div>
            ) : (
              <p className="text-muted-foreground text-xs">
                Project admins manage membership.
              </p>
            )}
            <ul className="mt-4 space-y-1">
              {members.map((m) => (
                <li
                  key={m.user_id}
                  className="flex items-center justify-between gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-muted/50"
                >
                  <span className="truncate font-medium">{m.username}</span>
                  {canAdmin ? (
                    <span className="flex shrink-0 items-center gap-1">
                      <select
                        aria-label={`Role for ${m.username}`}
                        className="h-7 rounded border border-input bg-transparent px-1 text-xs"
                        value={m.role}
                        onChange={(e) =>
                          void changeRole(m.user_id, e.target.value)
                        }
                      >
                        {roles.map((r) => (
                          <option key={r} value={r}>
                            {r}
                          </option>
                        ))}
                      </select>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 text-xs"
                        onClick={() => void removeMember(m.user_id)}
                      >
                        Remove
                      </Button>
                    </span>
                  ) : (
                    <span className="shrink-0 text-muted-foreground text-xs">
                      {m.role}
                    </span>
                  )}
                </li>
              ))}
              {members.length === 0 ? (
                <p className="text-muted-foreground text-xs">No members yet.</p>
              ) : null}
            </ul>
          </Card>

          {canAdmin ? (
            <Card
              title="Secrets"
              description="Injected as environment variables into every step, unless a step narrows them with secrets:"
              icon={KeyRound}
            >
              <div className="flex flex-wrap gap-2">
                <Input
                  className="max-w-[150px] font-mono text-xs"
                  placeholder="KEY"
                  value={secretKey}
                  onChange={(e) => setSecretKey(e.target.value)}
                />
                <Input
                  className="max-w-[190px]"
                  type="password"
                  placeholder="value"
                  value={secretValue}
                  onChange={(e) => setSecretValue(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && void saveSecret()}
                />
                <Button variant="outline" onClick={() => void saveSecret()}>
                  Save
                </Button>
              </div>
              <ul className="mt-4 space-y-1">
                {secrets.map((s) => (
                  <li
                    key={s.id}
                    className="flex items-center justify-between rounded-md px-2 py-1.5 text-sm hover:bg-muted/50"
                  >
                    <code className="truncate text-xs">{s.key}</code>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-7 text-xs"
                      onClick={() => void removeSecret(s.key)}
                    >
                      Delete
                    </Button>
                  </li>
                ))}
                {secrets.length === 0 ? (
                  <p className="text-muted-foreground text-xs">
                    No secrets set. Values are encrypted at rest when
                    FIBER_SECRETS_KEY is configured.
                  </p>
                ) : null}
              </ul>
            </Card>
          ) : null}

          {canAdmin ? (
            <Card
              title="GitHub webhook"
              description="Push and pull_request events. For PR path filters, also add a GITHUB_TOKEN secret with contents:read."
              icon={Webhook}
            >
              <code className="block break-all rounded-md bg-muted/40 px-3 py-2 font-mono text-[11px]">
                {api.apiUrl}/api/projects/{projectId}/webhooks/github
              </code>
              <div className="mt-3 flex gap-2">
                <Input
                  type="password"
                  placeholder="Webhook secret"
                  value={webhookSecret}
                  onChange={(e) => setWebhookSecret(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && void saveWebhook()}
                />
                <Button variant="outline" onClick={() => void saveWebhook()}>
                  Save
                </Button>
              </div>
              <p className="mt-2 text-[11px] text-muted-foreground">
                Webhooks fail closed: without a secret set here, deliveries are
                rejected.
              </p>
            </Card>
          ) : null}
        </div>
      </div>
    </AppShell>
  )
}
