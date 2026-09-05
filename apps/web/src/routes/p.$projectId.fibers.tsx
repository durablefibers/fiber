import { createFileRoute } from "@tanstack/react-router"
import { useCallback, useEffect, useState } from "react"
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
import { api, type DurableFiber, statusColor } from "@/lib/api"

export const Route = createFileRoute("/p/$projectId/fibers")({
  component: ProjectFibersPage,
})

const TASKS = [
  { name: "ping", hint: '{"message":"hello"}' },
  { name: "sleep_demo", hint: '{"seconds":3}' },
  { name: "interval_task", hint: '{"interval_seconds":60}' },
] as const

function ProjectFibersPage() {
  const { projectId } = Route.useParams()
  const [projectName, setProjectName] = useState<string>()
  const [fibers, setFibers] = useState<DurableFiber[]>([])
  const [task, setTask] = useState<string>("ping")
  const [input, setInput] = useState('{"message":"hello"}')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    void api.getProject(projectId).then((p) => setProjectName(p.name))
  }, [projectId])

  const load = useCallback(async () => {
    try {
      setFibers(await api.listFibers(projectId))
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load")
    }
  }, [projectId])

  useEffect(() => {
    void load()
    const t = setInterval(() => void load(), 2000)
    return () => clearInterval(t)
  }, [load])

  const create = async () => {
    setBusy(true)
    try {
      let parsed: unknown = {}
      try {
        parsed = JSON.parse(input || "{}")
      } catch {
        throw new Error("input must be valid JSON")
      }
      await api.createFiber(projectId, task, parsed)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Create failed")
    } finally {
      setBusy(false)
    }
  }

  const cancel = async (id: string) => {
    try {
      await api.cancelFiber(id)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Cancel failed")
    }
  }

  return (
    <AppShell projectId={projectId} projectName={projectName}>
      <header className="border-border/70 border-b px-8 py-5">
        <h1 className="font-semibold text-xl">Fibers</h1>
        <p className="text-muted-foreground text-sm">
          Durable control-plane tasks with step / stash / sleep checkpoints
        </p>
      </header>
      <div className="space-y-6 overflow-auto px-8 py-6">
        {error ? <p className="text-destructive text-sm">{error}</p> : null}

        <div className="flex flex-wrap items-end gap-2">
          <label className="space-y-1 text-sm">
            <span className="text-muted-foreground">Task</span>
            <select
              className="flex h-9 w-[160px] rounded-md border border-input bg-transparent px-3 text-sm"
              value={task}
              onChange={(e) => {
                const next = e.target.value
                setTask(next)
                const preset = TASKS.find((t) => t.name === next)
                if (preset) setInput(preset.hint)
              }}
            >
              {TASKS.map((t) => (
                <option key={t.name} value={t.name}>
                  {t.name}
                </option>
              ))}
            </select>
          </label>
          <label className="min-w-[240px] flex-1 space-y-1 text-sm">
            <span className="text-muted-foreground">Input (JSON)</span>
            <Input
              value={input}
              onChange={(e) => setInput(e.target.value)}
              className="font-mono text-xs"
            />
          </label>
          <Button onClick={() => void create()} disabled={busy}>
            Start fiber
          </Button>
        </div>

        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Attempts</TableHead>
              <TableHead>Wake</TableHead>
              <TableHead>Updated</TableHead>
              <TableHead />
            </TableRow>
          </TableHeader>
          <TableBody>
            {fibers.length === 0 ? (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground">
                  No fibers yet
                </TableCell>
              </TableRow>
            ) : (
              fibers.map((f) => (
                <TableRow key={f.id}>
                  <TableCell className="font-medium">
                    <div>{f.name}</div>
                    <div className="font-mono text-[10px] text-muted-foreground">
                      {f.id.slice(0, 8)}…
                    </div>
                  </TableCell>
                  <TableCell>
                    <Badge
                      variant="outline"
                      style={{ borderColor: statusColor(f.status) }}
                    >
                      {f.status}
                    </Badge>
                    {f.error ? (
                      <div className="mt-1 max-w-xs truncate text-destructive text-xs">
                        {f.error}
                      </div>
                    ) : null}
                    {f.result != null ? (
                      <div className="mt-1 max-w-xs truncate font-mono text-[10px] text-muted-foreground">
                        {JSON.stringify(f.result)}
                      </div>
                    ) : null}
                  </TableCell>
                  <TableCell>{f.attempts}</TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {f.wake_at
                      ? new Date(f.wake_at).toLocaleString()
                      : "—"}
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {new Date(f.updated_at).toLocaleString()}
                  </TableCell>
                  <TableCell>
                    {f.status === "pending" ||
                    f.status === "running" ||
                    f.status === "suspended" ? (
                      <Button
                        size="sm"
                        variant="outline"
                        onClick={() => void cancel(f.id)}
                      >
                        Cancel
                      </Button>
                    ) : null}
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </div>
    </AppShell>
  )
}
