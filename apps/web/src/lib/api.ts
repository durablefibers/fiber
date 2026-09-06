const API_URL =
  (typeof import.meta !== "undefined" &&
    (import.meta as { env?: { VITE_FIBER_API_URL?: string } }).env
      ?.VITE_FIBER_API_URL) ||
  "http://127.0.0.1:18080"

function wsBase() {
  const u = new URL(API_URL)
  u.protocol = u.protocol === "https:" ? "wss:" : "ws:"
  return u.origin
}

export type PublicUser = {
  id: string
  username: string
  /** Instance admin: global agent pool + user creation. Not a project-role bypass. */
  is_admin: boolean
}

export type Project = {
  id: string
  name: string
  slug: string
  created_at: string
  /** Present on getProject when membership is resolved. */
  role?: string
}

export type ProjectMember = {
  project_id: string
  user_id: string
  role: string
  username: string
  created_at: string
}

export type Pipeline = {
  id: string
  project_id: string
  name: string
  definition: PipelineDefinition
  created_at: string
  updated_at: string
}

export type PipelineDefinition = {
  name: string
  workspace?: {
    repo: string
    ref?: string
  }
  on?: {
    push?: {
      branches?: string[]
      paths?: string[]
      paths_ignore?: string[]
    }
    pull_request?: {
      branches?: string[]
      types?: string[]
      paths?: string[]
      paths_ignore?: string[]
    }
    interval_minutes?: number
    /** 6-field cron with seconds: `SEC MIN HOUR DAY MONTH DOW` */
    cron?: string
  }
  steps: StepDefinition[]
  /** Whole-run wall-clock limit in minutes. */
  timeout_minutes?: number
}

export type StepDefinition = {
  id: string
  name: string
  needs: string[]
  run?: string
  image?: string
  labels?: string[]
  retries?: number
  artifacts?: string[]
  /** Axis → values; expanded into multiple step runs at compile time. */
  matrix?: Record<string, string[]>
  /** `success()` (default), `always()`, `never()`, or `matrix.os == 'linux'`. */
  if?: string
  /** Per-attempt wall-clock limit in minutes (server default 60 when unset). */
  timeout_minutes?: number
  /** Project secrets to inject, by name. Omitted = all of them; `[]` = none. */
  secrets?: string[]
}

export type Artifact = {
  id: string
  run_id: string
  step_run_id: string
  name: string
  size: number
  created_at: string
}

export type DurableFiber = {
  id: string
  project_id: string
  name: string
  status: string
  input: unknown
  state: {
    steps?: Record<string, unknown>
    data?: Record<string, unknown>
    sleeps_done?: number
  }
  result?: unknown
  error?: string | null
  attempts: number
  wake_at?: string | null
  heartbeat_at?: string | null
  created_at: string
  updated_at: string
}

export type Run = {
  id: string
  pipeline_id: string
  project_id: string
  status: string
  trigger: string
  definition_snapshot: unknown
  created_at: string
  started_at?: string
  finished_at?: string
  /** Set when this run was created by retrying another. */
  retry_of?: string | null
}

/** One page of runs, newest first. Pass `next_cursor` back as `before` for the next. */
export type RunPage = {
  items: Run[]
  next_cursor: string | null
}

export type StepRun = {
  id: string
  run_id: string
  step_id: string
  step_name: string
  status: string
  image?: string
  run_cmd: string
  labels: string[]
  needs: string[]
  retries: number
  attempt: number
  exit_code?: number
  error?: string
  started_at?: string
  finished_at?: string
  /** Agent currently holding the step, and when its lease expires. */
  agent_id?: string | null
  lease_expires_at?: string | null
}

export type Agent = {
  id: string
  project_id?: string | null
  name: string
  labels: string[]
  concurrency: number
  last_seen_at?: string
  online: boolean
  created_at: string
}

export type StepAttempt = {
  id: string
  step_run_id: string
  attempt: number
  agent_id?: string | null
  started_at: string
  finished_at?: string | null
  status: string
  exit_code?: number | null
  error?: string | null
}

export type LogLine = {
  id: number
  run_id: string
  step_run_id: string
  stream: string
  data: string
  seq: number
  created_at: string
  /** Which attempt produced the line; `seq` restarts per attempt. */
  attempt: number
}

const TOKEN_KEY = "fiber_session_token"

export function getToken(): string | null {
  if (typeof localStorage === "undefined") return null
  return localStorage.getItem(TOKEN_KEY)
}

export function setToken(token: string | null) {
  if (typeof localStorage === "undefined") return
  if (token) localStorage.setItem(TOKEN_KEY, token)
  else localStorage.removeItem(TOKEN_KEY)
}

export type SecretMeta = {
  id: string
  project_id: string
  key: string
  created_at: string
  updated_at: string
}

/** Unwrap the server's `{ "error": "..." }` envelope; fall back to the raw body. */
function errorMessage(body: string): string {
  try {
    const parsed = JSON.parse(body) as { error?: unknown }
    if (parsed && typeof parsed.error === "string") return parsed.error
  } catch {
    // not JSON
  }
  return body
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken()
  const res = await fetch(`${API_URL}${path}`, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(init?.headers ?? {}),
    },
  })
  if (res.status === 401 && !path.startsWith("/api/auth/login")) {
    setToken(null)
    if (
      typeof window !== "undefined" &&
      !window.location.pathname.startsWith("/login")
    ) {
      window.location.href = "/login"
    }
  }
  if (!res.ok) {
    const body = await res.text()
    throw new Error(errorMessage(body) || res.statusText)
  }
  return res.json() as Promise<T>
}

export const api = {
  login: (username: string, password: string) =>
    request<{ token: string; user: PublicUser; expires_at: string }>(
      "/api/auth/login",
      {
        method: "POST",
        body: JSON.stringify({ username, password }),
      }
    ),
  logout: () =>
    request<{ ok: boolean }>("/api/auth/logout", { method: "POST" }),
  me: () => request<PublicUser>("/api/auth/me"),
  listUsers: () => request<PublicUser[]>("/api/users"),
  setUserAdmin: (id: string, is_admin: boolean) =>
    request<PublicUser>(`/api/users/${id}`, {
      method: "PUT",
      body: JSON.stringify({ is_admin }),
    }),
  listProjects: () => request<Project[]>("/api/projects"),
  createProject: (name: string) =>
    request<Project>("/api/projects", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  getProject: (id: string) => request<Project>(`/api/projects/${id}`),
  listMembers: (projectId: string) =>
    request<ProjectMember[]>(`/api/projects/${projectId}/members`),
  addMember: (
    projectId: string,
    username: string,
    role: string,
    password?: string
  ) =>
    request<{ ok: boolean; user_id: string; role: string }>(
      `/api/projects/${projectId}/members`,
      {
        method: "POST",
        body: JSON.stringify({ username, role, password }),
      }
    ),
  updateMember: (projectId: string, userId: string, role: string) =>
    request<{ ok: boolean }>(`/api/projects/${projectId}/members/${userId}`, {
      method: "PUT",
      body: JSON.stringify({ role }),
    }),
  removeMember: (projectId: string, userId: string) =>
    request<{ ok: boolean }>(`/api/projects/${projectId}/members/${userId}`, {
      method: "DELETE",
    }),
  listPipelines: (projectId: string) =>
    request<Pipeline[]>(`/api/projects/${projectId}/pipelines`),
  createPipeline: (
    projectId: string,
    name: string,
    definition: PipelineDefinition
  ) =>
    request<Pipeline>(`/api/projects/${projectId}/pipelines`, {
      method: "POST",
      body: JSON.stringify({ name, definition }),
    }),
  getPipeline: (id: string) => request<Pipeline>(`/api/pipelines/${id}`),
  updatePipeline: (id: string, definition: PipelineDefinition, name?: string) =>
    request<Pipeline>(`/api/pipelines/${id}`, {
      method: "PUT",
      body: JSON.stringify({ name, definition }),
    }),
  parseYaml: (yaml: string) =>
    request<PipelineDefinition>("/api/pipelines/parse-yaml", {
      method: "POST",
      body: JSON.stringify({ yaml }),
    }),
  startRun: (pipelineId: string) =>
    request<{ run: Run; steps: StepRun[] }>(
      `/api/pipelines/${pipelineId}/runs`,
      {
        method: "POST",
        body: JSON.stringify({ trigger: "manual" }),
      }
    ),
  listRuns: (projectId: string, opts?: { limit?: number; before?: string }) => {
    const q = new URLSearchParams()
    if (opts?.limit) q.set("limit", String(opts.limit))
    if (opts?.before) q.set("before", opts.before)
    const qs = q.toString()
    return request<RunPage>(
      `/api/projects/${projectId}/runs${qs ? `?${qs}` : ""}`
    )
  },
  getRun: (id: string) =>
    request<{ run: Run; steps: StepRun[] }>(`/api/runs/${id}`),
  cancelRun: (id: string) =>
    request<Run>(`/api/runs/${id}/cancel`, { method: "POST" }),
  /** Re-run from the original run's snapshot. `failed_only` carries succeeded steps over. */
  retryRun: (id: string, failedOnly = false) =>
    request<{ run: Run; steps: StepRun[] }>(`/api/runs/${id}/retry`, {
      method: "POST",
      body: JSON.stringify({ failed_only: failedOnly }),
    }),
  /** Without `after_id`, returns the newest `limit` lines. */
  listLogs: (
    stepRunId: string,
    opts?: { attempt?: number; afterId?: number; limit?: number }
  ) => {
    const q = new URLSearchParams()
    if (opts?.attempt !== undefined) q.set("attempt", String(opts.attempt))
    if (opts?.afterId !== undefined) q.set("after_id", String(opts.afterId))
    if (opts?.limit) q.set("limit", String(opts.limit))
    const qs = q.toString()
    return request<LogLine[]>(
      `/api/steps/${stepRunId}/logs${qs ? `?${qs}` : ""}`
    )
  },
  listStepAttempts: (stepRunId: string) =>
    request<StepAttempt[]>(`/api/steps/${stepRunId}/attempts`),
  listArtifacts: (runId: string) =>
    request<Artifact[]>(`/api/runs/${runId}/artifacts`),
  downloadArtifact: async (id: string, filename: string) => {
    const token = getToken()
    const res = await fetch(`${API_URL}/api/artifacts/${id}/download`, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      redirect: "follow",
    })
    if (!res.ok) throw new Error(await res.text())
    const blob = await res.blob()
    const url = URL.createObjectURL(blob)
    const a = document.createElement("a")
    a.href = url
    a.download = filename
    a.click()
    URL.revokeObjectURL(url)
  },
  listFibers: (projectId: string) =>
    request<DurableFiber[]>(`/api/projects/${projectId}/fibers`),
  createFiber: (
    projectId: string,
    name: string,
    input: unknown = {},
    wake_at?: string
  ) =>
    request<DurableFiber>(`/api/projects/${projectId}/fibers`, {
      method: "POST",
      body: JSON.stringify({ name, input, wake_at }),
    }),
  getFiber: (id: string) => request<DurableFiber>(`/api/fibers/${id}`),
  cancelFiber: (id: string) =>
    request<DurableFiber>(`/api/fibers/${id}/cancel`, { method: "POST" }),
  listAgents: (projectId?: string) =>
    request<Agent[]>(
      projectId
        ? `/api/agents?project_id=${encodeURIComponent(projectId)}`
        : "/api/agents"
    ),
  createAgent: (
    name: string,
    labels: string[],
    concurrency = 1,
    projectId?: string | null
  ) =>
    request<{ agent: Agent; token: string }>("/api/agents", {
      method: "POST",
      body: JSON.stringify({
        name,
        labels,
        concurrency,
        ...(projectId ? { project_id: projectId } : {}),
      }),
    }),
  updateAgent: (
    id: string,
    patch: { name?: string; labels?: string[]; concurrency?: number }
  ) =>
    request<Agent>(`/api/agents/${id}`, {
      method: "PUT",
      body: JSON.stringify(patch),
    }),
  deleteAgent: (id: string) =>
    request<{ ok: boolean }>(`/api/agents/${id}`, { method: "DELETE" }),
  rotateAgentToken: (id: string) =>
    request<{ agent: Agent; token: string }>(`/api/agents/${id}/rotate-token`, {
      method: "POST",
    }),
  setGithubSecret: (projectId: string, secret: string) =>
    request<{ ok: boolean }>(`/api/projects/${projectId}/webhooks/github`, {
      method: "PUT",
      body: JSON.stringify({ secret }),
    }),
  listSecrets: (projectId: string) =>
    request<SecretMeta[]>(`/api/projects/${projectId}/secrets`),
  upsertSecret: (projectId: string, key: string, value: string) =>
    request<SecretMeta>(`/api/projects/${projectId}/secrets`, {
      method: "POST",
      body: JSON.stringify({ key, value }),
    }),
  deleteSecret: (projectId: string, key: string) =>
    request<{ ok: boolean }>(
      `/api/projects/${projectId}/secrets/${encodeURIComponent(key)}`,
      { method: "DELETE" }
    ),
  runEventsUrl: (runId: string) => {
    const token = getToken()
    const base = `${wsBase()}/ws/runs/${runId}`
    return token ? `${base}?token=${encodeURIComponent(token)}` : base
  },
  apiUrl: API_URL,
}

export { emptyDefinition } from "@/lib/templates"

export function statusColor(status: string): string {
  switch (status) {
    case "succeeded":
      return "#22c55e"
    case "failed":
      return "#ef4444"
    case "running":
      return "#3b82f6"
    case "queued":
      return "#eab308"
    case "skipped":
    case "cancelled":
      return "#6b7280"
    default:
      return "#9ca3af"
  }
}

/** Export definition as fiber.yml-style YAML (map steps). */
export function definitionToYaml(def: PipelineDefinition): string {
  const lines: string[] = [`name: ${yamlQuote(def.name)}`]
  if (def.workspace?.repo) {
    lines.push("workspace:")
    lines.push(`  repo: ${yamlQuote(def.workspace.repo)}`)
    if (def.workspace.ref) {
      lines.push(`  ref: ${yamlQuote(def.workspace.ref)}`)
    }
  }
  if (
    def.on?.push?.branches?.length ||
    def.on?.push?.paths?.length ||
    def.on?.push?.paths_ignore?.length ||
    def.on?.pull_request ||
    def.on?.interval_minutes ||
    def.on?.cron
  ) {
    lines.push("on:")
    if (
      def.on.push?.branches?.length ||
      def.on.push?.paths?.length ||
      def.on.push?.paths_ignore?.length
    ) {
      lines.push("  push:")
      if (def.on.push.branches?.length) {
        lines.push(
          `    branches: [${def.on.push.branches.map(yamlQuote).join(", ")}]`
        )
      }
      if (def.on.push.paths?.length) {
        lines.push(
          `    paths: [${def.on.push.paths.map(yamlQuote).join(", ")}]`
        )
      }
      if (def.on.push.paths_ignore?.length) {
        lines.push(
          `    paths_ignore: [${def.on.push.paths_ignore.map(yamlQuote).join(", ")}]`
        )
      }
    }
    if (def.on.pull_request) {
      const pr = def.on.pull_request
      lines.push("  pull_request:")
      if (pr.branches?.length) {
        lines.push(`    branches: [${pr.branches.map(yamlQuote).join(", ")}]`)
      }
      if (pr.types?.length) {
        lines.push(`    types: [${pr.types.map(yamlQuote).join(", ")}]`)
      }
      if (pr.paths?.length) {
        lines.push(`    paths: [${pr.paths.map(yamlQuote).join(", ")}]`)
      }
      if (pr.paths_ignore?.length) {
        lines.push(
          `    paths_ignore: [${pr.paths_ignore.map(yamlQuote).join(", ")}]`
        )
      }
    }
    if (def.on.interval_minutes) {
      lines.push(`  interval_minutes: ${def.on.interval_minutes}`)
    }
    if (def.on.cron) {
      lines.push(`  cron: ${yamlQuote(def.on.cron)}`)
    }
  }
  if (def.timeout_minutes) {
    lines.push(`timeout_minutes: ${def.timeout_minutes}`)
  }
  lines.push("steps:")
  for (const s of def.steps) {
    lines.push(`  ${s.id}:`)
    if (s.name && s.name !== s.id) lines.push(`    name: ${yamlQuote(s.name)}`)
    if (s.needs?.length) {
      lines.push(`    needs: [${s.needs.map(yamlQuote).join(", ")}]`)
    }
    if (s.image) lines.push(`    image: ${yamlQuote(s.image)}`)
    if (s.labels?.length) {
      lines.push(`    labels: [${s.labels.map(yamlQuote).join(", ")}]`)
    }
    if (s.retries) lines.push(`    retries: ${s.retries}`)
    if (s.timeout_minutes) {
      lines.push(`    timeout_minutes: ${s.timeout_minutes}`)
    }
    if (s.secrets) {
      lines.push(`    secrets: [${s.secrets.map(yamlQuote).join(", ")}]`)
    }
    if (s.if) lines.push(`    if: ${yamlQuote(s.if)}`)
    if (s.matrix && Object.keys(s.matrix).length) {
      lines.push("    matrix:")
      for (const [axis, values] of Object.entries(s.matrix)) {
        lines.push(`      ${axis}: [${values.map(yamlQuote).join(", ")}]`)
      }
    }
    if (s.artifacts?.length) {
      lines.push(`    artifacts: [${s.artifacts.map(yamlQuote).join(", ")}]`)
    }
    if (s.run) {
      if (s.run.includes("\n")) {
        lines.push("    run: |")
        for (const line of s.run.split("\n")) lines.push(`      ${line}`)
      } else {
        lines.push(`    run: ${yamlQuote(s.run)}`)
      }
    }
  }
  return `${lines.join("\n")}\n`
}

function yamlQuote(s: string): string {
  if (/^[\w./:@-]+$/.test(s)) return s
  return JSON.stringify(s)
}
