import { createFileRoute } from "@tanstack/react-router"
import { useState } from "react"
import { AppShell } from "@/components/app-shell"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { api } from "@/lib/api"

export const Route = createFileRoute("/settings")({ component: SettingsPage })

function SettingsPage() {
  const [projectId, setProjectId] = useState("")
  const [secret, setSecret] = useState("")
  const [msg, setMsg] = useState<string | null>(null)

  const save = async () => {
    try {
      await api.setGithubSecret(projectId, secret)
      setMsg("GitHub webhook secret saved.")
    } catch (e) {
      setMsg(e instanceof Error ? e.message : "Failed")
    }
  }

  return (
    <AppShell>
      <header className="border-border/70 border-b px-8 py-5">
        <h1 className="font-semibold text-xl">Settings</h1>
        <p className="text-muted-foreground text-sm">
          Webhooks and control-plane configuration
        </p>
      </header>
      <div className="max-w-xl space-y-4 px-8 py-6">
        <div>
          <label className="text-muted-foreground text-xs">Project ID</label>
          <Input
            value={projectId}
            onChange={(e) => setProjectId(e.target.value)}
          />
        </div>
        <div>
          <label className="text-muted-foreground text-xs">
            GitHub webhook secret
          </label>
          <Input
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
            type="password"
          />
        </div>
        <Button onClick={() => void save()}>Save webhook secret</Button>
        {msg ? <p className="text-muted-foreground text-sm">{msg}</p> : null}
        <div className="rounded-lg border border-border/60 p-4 text-muted-foreground text-sm">
          <p className="font-medium text-foreground">Webhook URL</p>
          <code className="mt-2 block break-all text-xs">
            {api.apiUrl}/api/projects/&lt;projectId&gt;/webhooks/github
          </code>
        </div>
      </div>
    </AppShell>
  )
}
