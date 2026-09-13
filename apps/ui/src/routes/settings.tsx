import { createFileRoute } from "@tanstack/react-router"
import { KeyRound, LogOut, ShieldCheck, UserPlus } from "lucide-react"
import { useEffect, useState } from "react"
import { toast } from "sonner"
import { AppShell } from "@/components/app-shell"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { api, type PublicUser } from "@/lib/api"

export const Route = createFileRoute("/settings")({ component: SettingsPage })

function Section({
  title,
  description,
  icon: Icon,
  children,
}: {
  title: string
  description: string
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

function SettingsPage() {
  const [me, setMe] = useState<PublicUser | null>(null)
  const [users, setUsers] = useState<PublicUser[]>([])
  const [current, setCurrent] = useState("")
  const [next, setNext] = useState("")
  const [confirm, setConfirm] = useState("")
  const [busy, setBusy] = useState(false)
  const [newName, setNewName] = useState("")
  const [newPassword, setNewPassword] = useState("")

  const loadUsers = async () => {
    try {
      setUsers(await api.listUsers())
    } catch {
      // Not an instance admin — the section stays hidden.
      setUsers([])
    }
  }

  // biome-ignore lint/correctness/useExhaustiveDependencies: load once on mount
  useEffect(() => {
    void (async () => {
      try {
        const user = await api.me()
        setMe(user)
        if (user.is_admin) await loadUsers()
      } catch (e) {
        toast.error(e instanceof Error ? e.message : "Could not load account")
      }
    })()
  }, [])

  const changePassword = async () => {
    if (next.length < 8) {
      toast.error("New password must be at least 8 characters.")
      return
    }
    if (next !== confirm) {
      toast.error("New passwords do not match.")
      return
    }
    setBusy(true)
    try {
      await api.changePassword(current, next)
      setCurrent("")
      setNext("")
      setConfirm("")
      toast.success("Password changed — other sessions signed out.")
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "Could not change password")
    } finally {
      setBusy(false)
    }
  }

  const revoke = async () => {
    try {
      const { revoked } = await api.revokeSessions()
      toast.success(
        revoked === 0
          ? "No other sessions were open."
          : `Signed out ${revoked} other session${revoked === 1 ? "" : "s"}.`
      )
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "Could not revoke sessions")
    }
  }

  const createUser = async () => {
    if (!newName.trim() || newPassword.length < 8) {
      toast.error("Username required; password must be at least 8 characters.")
      return
    }
    try {
      await api.createUser(newName.trim(), newPassword)
      setNewName("")
      setNewPassword("")
      await loadUsers()
      toast.success("User created.")
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "Could not create user")
    }
  }

  // The server refuses only the last admin's demotion (store::set_instance_admin), so
  // that is the one case to disable — self-demotion is allowed while someone else holds
  // the flag.
  const adminCount = users.filter((u) => u.is_admin).length
  const cannotDemote = (u: PublicUser) => u.is_admin && adminCount <= 1

  const toggleAdmin = async (u: PublicUser) => {
    try {
      await api.setUserAdmin(u.id, !u.is_admin)
      await loadUsers()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "Could not update user")
    }
  }

  return (
    <AppShell>
      <header className="shrink-0 border-border/70 border-b px-8 py-5">
        <h1 className="font-semibold text-xl tracking-tight">Settings</h1>
        <p className="text-muted-foreground text-sm">
          Your account{me?.is_admin ? " and instance users" : ""}. Project
          secrets and webhooks live under each project.
        </p>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">
        <div className="grid max-w-5xl gap-5 lg:grid-cols-2">
          <Section
            title="Account"
            description={
              me
                ? `Signed in as ${me.username}${me.is_admin ? " · instance admin" : ""}`
                : "Loading…"
            }
            icon={KeyRound}
          >
            <div className="space-y-3">
              <Input
                type="password"
                autoComplete="current-password"
                placeholder="Current password"
                value={current}
                onChange={(e) => setCurrent(e.target.value)}
              />
              <Input
                type="password"
                autoComplete="new-password"
                placeholder="New password (min 8 characters)"
                value={next}
                onChange={(e) => setNext(e.target.value)}
              />
              <Input
                type="password"
                autoComplete="new-password"
                placeholder="Confirm new password"
                value={confirm}
                onChange={(e) => setConfirm(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && void changePassword()}
              />
              <Button
                onClick={() => void changePassword()}
                disabled={busy || !current || !next}
              >
                {busy ? "Changing…" : "Change password"}
              </Button>
            </div>
          </Section>

          <Section
            title="Sessions"
            description="Bearer sessions issued at login. Changing your password revokes the others automatically."
            icon={LogOut}
          >
            <Button variant="outline" onClick={() => void revoke()}>
              Sign out other sessions
            </Button>
          </Section>

          {me?.is_admin ? (
            <Section
              title="Users"
              description="Instance admins manage the global agent pool and can create users. It is not a project-role bypass."
              icon={ShieldCheck}
            >
              <div className="flex flex-wrap gap-2">
                <Input
                  className="max-w-[160px]"
                  placeholder="username"
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                />
                <Input
                  className="max-w-[180px]"
                  type="password"
                  placeholder="password (min 8)"
                  value={newPassword}
                  onChange={(e) => setNewPassword(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && void createUser()}
                />
                <Button variant="outline" onClick={() => void createUser()}>
                  <UserPlus className="size-4" />
                  Create
                </Button>
              </div>
              <ul className="mt-4 space-y-1">
                {users.map((u) => (
                  <li
                    key={u.id}
                    className="flex items-center justify-between gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-muted/50"
                  >
                    <span className="flex min-w-0 items-center gap-2">
                      <span className="truncate font-medium">{u.username}</span>
                      {u.is_admin ? (
                        <Badge
                          variant="secondary"
                          className="h-5 bg-sky-500/15 text-[10px] text-sky-300"
                        >
                          admin
                        </Badge>
                      ) : null}
                      {u.id === me.id ? (
                        <span className="text-[10px] text-muted-foreground">
                          you
                        </span>
                      ) : null}
                    </span>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-7 text-xs"
                      disabled={cannotDemote(u)}
                      title={
                        cannotDemote(u)
                          ? "An instance needs at least one admin"
                          : undefined
                      }
                      onClick={() => void toggleAdmin(u)}
                    >
                      {u.is_admin ? "Revoke admin" : "Make admin"}
                    </Button>
                  </li>
                ))}
              </ul>
            </Section>
          ) : null}

          <Section
            title="API"
            description="What this UI talks to. Agents and the CLI use the same base URL."
            icon={ShieldCheck}
          >
            <code className="block break-all rounded-md bg-muted/40 px-3 py-2 font-mono text-xs">
              {api.apiUrl}
            </code>
          </Section>
        </div>
      </div>
    </AppShell>
  )
}
