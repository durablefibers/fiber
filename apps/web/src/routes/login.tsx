import { createFileRoute, useRouter } from "@tanstack/react-router"
import { Workflow } from "lucide-react"
import { useState } from "react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { api, setToken } from "@/lib/api"

export const Route = createFileRoute("/login")({
  component: LoginPage,
})

function LoginPage() {
  const router = useRouter()
  const [username, setUsername] = useState("admin")
  const [password, setPassword] = useState("fiber")
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const submit = async () => {
    setLoading(true)
    setError(null)
    try {
      const res = await api.login(username, password)
      setToken(res.token)
      await router.navigate({ to: "/" })
    } catch (e) {
      setError(e instanceof Error ? e.message : "Login failed")
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="flex min-h-svh items-center justify-center bg-[radial-gradient(ellipse_at_top,_oklch(0.22_0.03_250),_oklch(0.14_0.01_260)_60%)] p-6">
      <div className="w-full max-w-sm rounded-xl border border-white/10 bg-black/30 p-6 backdrop-blur">
        <div className="mb-6 flex items-center gap-2">
          <Workflow className="size-5 text-sky-400" />
          <div>
            <div className="font-semibold">Fiber</div>
            <div className="text-white/50 text-xs">Sign in to continue</div>
          </div>
        </div>
        <div className="space-y-3">
          <div>
            <label className="text-white/50 text-xs">Username</label>
            <Input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoComplete="username"
            />
          </div>
          <div>
            <label className="text-white/50 text-xs">Password</label>
            <Input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && void submit()}
              autoComplete="current-password"
            />
          </div>
          {error ? <p className="text-red-400 text-sm">{error}</p> : null}
          <Button
            className="w-full"
            disabled={loading}
            onClick={() => void submit()}
          >
            {loading ? "Signing in…" : "Sign in"}
          </Button>
          <p className="text-center text-[11px] text-white/35">
            Default: admin / fiber (FIBER_ADMIN_*)
          </p>
        </div>
      </div>
    </div>
  )
}
