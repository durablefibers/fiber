import { createFileRoute } from "@tanstack/react-router"
import { Copy, KeyRound, Pencil, Trash2 } from "lucide-react"
import { useEffect, useState } from "react"
import { AppShell } from "@/components/app-shell"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { type Agent, api } from "@/lib/api"

export const Route = createFileRoute("/agents")({ component: AgentsPage })

function AgentsPage() {
  return (
    <AppShell>
      <AgentsContent />
    </AppShell>
  )
}

function relativeTime(iso?: string | null): string {
  if (!iso) return "—"
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return "—"
  const sec = Math.max(0, Math.floor((Date.now() - t) / 1000))
  if (sec < 5) return "just now"
  if (sec < 60) return `${sec}s ago`
  const min = Math.floor(sec / 60)
  if (min < 60) return `${min}m ago`
  const hr = Math.floor(min / 60)
  if (hr < 48) return `${hr}h ago`
  return new Date(iso).toLocaleString()
}

function agentPresence(a: Agent): "online" | "stale" | "offline" {
  if (!a.online) return "offline"
  if (!a.last_seen_at) return "stale"
  const age = Date.now() - new Date(a.last_seen_at).getTime()
  if (Number.isNaN(age) || age > 45_000) return "stale"
  return "online"
}

function labelsOf(a: Agent): string[] {
  return Array.isArray(a.labels) ? a.labels : []
}

function isScopedTo(a: Agent, projectId?: string): boolean {
  if (!projectId) return !a.project_id
  return a.project_id === projectId
}

export function AgentsContent({ projectId }: { projectId?: string }) {
  const [agents, setAgents] = useState<Agent[]>([])
  const [name, setName] = useState(projectId ? "project" : "local")
  const [labels, setLabels] = useState("os=linux,docker=true")
  const [concurrency, setConcurrency] = useState(1)
  const [token, setToken] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  const [editName, setEditName] = useState("")
  const [editLabels, setEditLabels] = useState("")
  const [editConcurrency, setEditConcurrency] = useState(1)
  const [, setTick] = useState(0)

  const load = async () => {
    try {
      setAgents(await api.listAgents(projectId))
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load")
    }
  }

  useEffect(() => {
    void load()
    const t = setInterval(() => void load(), 5000)
    const tick = setInterval(() => setTick((n) => n + 1), 1000)
    return () => {
      clearInterval(t)
      clearInterval(tick)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reload when project changes
  }, [projectId])

  const create = async () => {
    try {
      const res = await api.createAgent(
        name,
        labels
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean),
        Math.max(1, concurrency || 1),
        projectId ?? null
      )
      setToken(res.token)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Create failed")
    }
  }

  const startEdit = (a: Agent) => {
    setEditingId(a.id)
    setEditName(a.name)
    setEditLabels(labelsOf(a).join(", "))
    setEditConcurrency(a.concurrency)
  }

  const saveEdit = async () => {
    if (!editingId) return
    try {
      await api.updateAgent(editingId, {
        name: editName.trim() || undefined,
        labels: editLabels
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean),
        concurrency: Math.max(1, editConcurrency || 1),
      })
      setEditingId(null)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Update failed")
    }
  }

  const remove = async (a: Agent) => {
    if (
      !window.confirm(
        `Delete agent “${a.name}”? In-flight steps will be requeued.`
      )
    ) {
      return
    }
    try {
      await api.deleteAgent(a.id)
      if (editingId === a.id) setEditingId(null)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Delete failed")
    }
  }

  const rotate = async (a: Agent) => {
    if (
      !window.confirm(
        `Rotate token for “${a.name}”? The current token stops working and the agent must reconnect.`
      )
    ) {
      return
    }
    try {
      const res = await api.rotateAgentToken(a.id)
      setToken(res.token)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Rotate failed")
    }
  }

  const copyToken = async () => {
    if (!token) return
    try {
      await navigator.clipboard.writeText(token)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    } catch {
      /* ignore */
    }
  }

  const onlineCount = agents.filter((a) => agentPresence(a) === "online").length
  const canManage = (a: Agent) => {
    if (!projectId) return true
    return isScopedTo(a, projectId)
  }

  return (
    <>
      <header className="border-border/70 border-b px-8 py-5">
        <h1 className="font-semibold text-xl">
          {projectId ? "Project agents" : "Agents"}
        </h1>
        <p className="text-muted-foreground text-sm">
          {projectId
            ? "Scoped workers for this project, plus global agents that can pick up any step"
            : "Outbound WebSocket workers — global pool, or scope one to a project"}
          {agents.length > 0 ? (
            <span className="ml-2 text-foreground/80">
              · {onlineCount}/{agents.length} online
            </span>
          ) : null}
        </p>
      </header>
      <div className="space-y-6 px-8 py-6">
        {error ? <p className="text-destructive text-sm">{error}</p> : null}
        <div className="flex flex-wrap gap-2">
          <Input
            className="max-w-[140px]"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Name"
          />
          <Input
            className="max-w-xs"
            value={labels}
            onChange={(e) => setLabels(e.target.value)}
            placeholder="labels"
          />
          <Input
            className="w-20"
            type="number"
            min={1}
            value={concurrency}
            onChange={(e) => setConcurrency(Number(e.target.value) || 1)}
            title="Concurrency"
          />
          <Button onClick={() => void create()}>
            {projectId ? "Register project agent" : "Register global agent"}
          </Button>
        </div>
        {token ? (
          <div className="rounded-lg border border-amber-500/30 bg-amber-500/10 p-4 text-sm">
            <div className="flex items-center justify-between gap-2">
              <div className="font-medium">
                Agent token (copy now — shown once)
              </div>
              <Button
                size="sm"
                variant="outline"
                className="h-7"
                onClick={() => void copyToken()}
              >
                <Copy className="size-3.5" />
                {copied ? "Copied" : "Copy"}
              </Button>
            </div>
            <code className="mt-2 block break-all font-mono text-xs">
              {token}
            </code>
            <pre className="mt-3 overflow-x-auto rounded bg-black/80 p-3 font-mono text-[11px] text-emerald-200">
              {`FIBER_AGENT_TOKEN=${token} \\
FIBER_API_URL=ws://127.0.0.1:18080 \\
cargo run -p fiber-agent`}
            </pre>
          </div>
        ) : null}
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Pool</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Labels</TableHead>
              <TableHead>Concurrency</TableHead>
              <TableHead>Last seen</TableHead>
              <TableHead className="w-[100px]" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {agents.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={7}
                  className="text-muted-foreground text-sm"
                >
                  No agents yet. Register one, then run{" "}
                  <code className="text-xs">fiber-agent</code> with the token.
                </TableCell>
              </TableRow>
            ) : (
              agents.map((a) => {
                const presence = agentPresence(a)
                const editing = editingId === a.id
                const manage = canManage(a)
                const poolLabel = a.project_id
                  ? projectId && a.project_id === projectId
                    ? "project"
                    : "project"
                  : "global"
                return (
                  <TableRow key={a.id}>
                    <TableCell>
                      {editing ? (
                        <Input
                          className="h-8 max-w-[140px]"
                          value={editName}
                          onChange={(e) => setEditName(e.target.value)}
                        />
                      ) : (
                        <>
                          <div className="font-medium">{a.name}</div>
                          <div className="font-mono text-[10px] text-muted-foreground">
                            {a.id.slice(0, 8)}
                          </div>
                        </>
                      )}
                    </TableCell>
                    <TableCell>
                      <Badge variant="secondary">{poolLabel}</Badge>
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant={
                          presence === "online" ? "default" : "secondary"
                        }
                        className={
                          presence === "online"
                            ? "bg-emerald-600/80 hover:bg-emerald-600/80"
                            : presence === "stale"
                              ? "bg-amber-600/40 text-amber-100"
                              : undefined
                        }
                      >
                        {presence}
                      </Badge>
                    </TableCell>
                    <TableCell className="font-mono text-xs">
                      {editing ? (
                        <Input
                          className="h-8 min-w-[160px] font-mono text-xs"
                          value={editLabels}
                          onChange={(e) => setEditLabels(e.target.value)}
                        />
                      ) : (
                        labelsOf(a).join(", ") || "—"
                      )}
                    </TableCell>
                    <TableCell>
                      {editing ? (
                        <Input
                          className="h-8 w-16"
                          type="number"
                          min={1}
                          value={editConcurrency}
                          onChange={(e) =>
                            setEditConcurrency(Number(e.target.value) || 1)
                          }
                        />
                      ) : (
                        a.concurrency
                      )}
                    </TableCell>
                    <TableCell
                      className="text-muted-foreground text-xs"
                      title={a.last_seen_at ?? undefined}
                    >
                      {relativeTime(a.last_seen_at)}
                    </TableCell>
                    <TableCell>
                      <div className="flex justify-end gap-1">
                        {!manage ? (
                          <span className="text-muted-foreground text-[10px]">
                            manage on Agents
                          </span>
                        ) : editing ? (
                          <>
                            <Button
                              size="sm"
                              variant="outline"
                              className="h-7"
                              onClick={() => void saveEdit()}
                            >
                              Save
                            </Button>
                            <Button
                              size="sm"
                              variant="ghost"
                              className="h-7"
                              onClick={() => setEditingId(null)}
                            >
                              Cancel
                            </Button>
                          </>
                        ) : (
                          <>
                            <Button
                              size="sm"
                              variant="ghost"
                              className="h-7"
                              onClick={() => startEdit(a)}
                              title="Edit"
                            >
                              <Pencil className="size-3.5" />
                            </Button>
                            <Button
                              size="sm"
                              variant="ghost"
                              className="h-7"
                              onClick={() => void rotate(a)}
                              title="Rotate token"
                            >
                              <KeyRound className="size-3.5" />
                            </Button>
                            <Button
                              size="sm"
                              variant="ghost"
                              className="h-7 text-destructive hover:text-destructive"
                              onClick={() => void remove(a)}
                              title="Delete"
                            >
                              <Trash2 className="size-3.5" />
                            </Button>
                          </>
                        )}
                      </div>
                    </TableCell>
                  </TableRow>
                )
              })
            )}
          </TableBody>
        </Table>
      </div>
    </>
  )
}
