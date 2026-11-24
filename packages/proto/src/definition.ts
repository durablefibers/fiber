/** Pipeline definition types. Absent optional fields are `null`, never `undefined`. */
export type StepStatus =
  | "pending"
  | "queued"
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "skipped"
export type RunStatus = "pending" | "running" | "succeeded" | "failed" | "cancelled"

export const isTerminalStep = (s: StepStatus): boolean =>
  s === "succeeded" || s === "failed" || s === "cancelled" || s === "skipped"
export const isTerminalRun = (s: RunStatus): boolean =>
  s === "succeeded" || s === "failed" || s === "cancelled"

export interface StepDefinition {
  id: string
  name: string
  needs: string[]
  run: string | null
  image: string | null
  labels: string[]
  retries: number
  env: Record<string, string>
  working_directory: string | null
  shell: string | null
  continue_on_error: boolean
  artifacts: string[]
  matrix: Record<string, string[]> | null
  if: string | null
  timeout_minutes: number | null
  /** `null` = every project secret; `[]` = none. */
  secrets: string[] | null
}
export interface PushTrigger {
  branches: string[]
  paths: string[]
  paths_ignore: string[]
}
export interface PullRequestTrigger extends PushTrigger {
  types: string[]
}
export interface PipelineTriggers {
  push: PushTrigger | null
  pull_request: PullRequestTrigger | null
  interval_minutes: number | null
  cron: string | null
}
export interface WorkspaceConfig {
  repo: string
  ref: string | null
}
export interface ConcurrencyConfig {
  group: string | null
  cancel_in_progress: boolean
}
export interface PipelineDefinition {
  name: string
  env: Record<string, string>
  workspace: WorkspaceConfig | null
  on: PipelineTriggers | null
  steps: StepDefinition[]
  timeout_minutes: number | null
  concurrency: ConcurrencyConfig | null
}
export interface CompiledStep {
  id: string
  name: string
  needs: string[]
  run: string
  image: string | null
  labels: string[]
  retries: number
  artifacts: string[]
  level: number
  matrix: Record<string, string>
  env: [string, string][]
  if: string | null
  timeout_minutes: number | null
  secrets: string[] | null
  working_directory: string | null
  shell: string | null
  continue_on_error: boolean
}
export interface CompiledDag {
  name: string
  workspace: WorkspaceConfig | null
  steps: CompiledStep[]
  levels: string[][]
  timeout_minutes: number | null
}
