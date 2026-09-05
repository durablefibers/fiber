import { createFileRoute, Link, useRouter } from "@tanstack/react-router"
import { Plus, Sparkles } from "lucide-react"
import { useEffect, useMemo, useState } from "react"
import { AppShell } from "@/components/app-shell"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { api, type Project } from "@/lib/api"

export const Route = createFileRoute("/")({ component: ProjectsPage })

function ProjectsPage() {
  const [projects, setProjects] = useState<Project[]>([])
  const [name, setName] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const router = useRouter()

  const load = async () => {
    try {
      setProjects(await api.listProjects())
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load")
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    void load()
  }, [])

  const ordered = useMemo(() => {
    const showcase = projects.filter((p) => p.slug === "showcase")
    const rest = projects.filter((p) => p.slug !== "showcase")
    return [...showcase, ...rest]
  }, [projects])

  const create = async () => {
    if (!name.trim()) return
    try {
      const p = await api.createProject(name.trim())
      setName("")
      await router.navigate({
        to: "/p/$projectId",
        params: { projectId: p.id },
      })
    } catch (e) {
      setError(e instanceof Error ? e.message : "Create failed")
    }
  }

  return (
    <AppShell>
      <header className="border-border/70 border-b px-8 py-6">
        <h1 className="font-semibold text-2xl tracking-tight">Projects</h1>
        <p className="mt-1 text-muted-foreground text-sm">
          Pipeline canvas, agents, and runs — calm project UI, Jenkins-capable CI.
        </p>
      </header>
      <div className="flex flex-1 flex-col gap-6 px-8 py-6">
        <div className="flex max-w-lg gap-2">
          <Input
            placeholder="New project name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && void create()}
          />
          <Button onClick={() => void create()}>
            <Plus className="size-4" />
            Create
          </Button>
        </div>
        {error ? <p className="text-destructive text-sm">{error}</p> : null}
        {loading ? (
          <p className="text-muted-foreground text-sm">Loading…</p>
        ) : ordered.length === 0 ? (
          <p className="text-muted-foreground text-sm">
            No projects yet. Restart <code className="text-xs">fiber-api</code>{" "}
            to seed the Showcase examples, or create a project.
          </p>
        ) : (
          <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {ordered.map((p) => {
              const showcase = p.slug === "showcase"
              return (
                <li key={p.id}>
                  <Link
                    to="/p/$projectId"
                    params={{ projectId: p.id }}
                    className={`block rounded-xl border p-5 transition hover:border-sky-500/40 hover:bg-card ${
                      showcase
                        ? "border-sky-500/35 bg-sky-500/5"
                        : "border-border/70 bg-card/40"
                    }`}
                  >
                    <div className="flex items-center gap-2">
                      {showcase ? (
                        <Sparkles className="size-4 text-sky-400" />
                      ) : null}
                      <div className="font-medium text-base">{p.name}</div>
                      {showcase ? (
                        <Badge className="ml-auto bg-sky-500/20 text-sky-300 hover:bg-sky-500/20">
                          examples
                        </Badge>
                      ) : null}
                    </div>
                    <div className="mt-1 font-mono text-muted-foreground text-xs">
                      {p.slug}
                    </div>
                    {showcase ? (
                      <p className="mt-2 text-muted-foreground text-xs">
                        Diamond CI, fan-out tests, release train, retries,
                        nightly
                      </p>
                    ) : null}
                  </Link>
                </li>
              )
            })}
          </ul>
        )}
      </div>
    </AppShell>
  )
}
