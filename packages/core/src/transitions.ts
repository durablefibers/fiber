/**
 * The run state machine: pure decisions over a run's steps. The run's Durable Object applies
 * them in the same transaction as the event that caused them, so a run can never be left
 * with a recorded completion and unplanned dependents.
 */
import { isTerminalStep, type RunStatus, type StepStatus, trim } from "@fiber/proto"
import { evalIf } from "./step-if.ts"

export interface PlanStep {
  step_id: string
  status: StepStatus
  needs: readonly string[]
  if: string | null
  env: readonly (readonly [string, string])[]
  continue_on_error: boolean
}
export type Transition =
  | { index: number; to: "queued" }
  | { index: number; to: "skipped"; reason: string }

/** A tolerated failure is recorded as failed but does not propagate. A cancel always does. */
const broke = (s: PlanStep): boolean =>
  s.status === "cancelled" || (s.status === "failed" && !s.continue_on_error)

/** One pass: what must be skipped, and what has every dependency done and may be queued. */
export function planTransitions(steps: readonly PlanStep[]): Transition[] {
  const ok = new Set<string>()
  const terminal = new Set<string>()
  const tainted = new Set<string>()
  for (const s of steps) {
    if (broke(s)) tainted.add(s.step_id)
    else if (s.status === "succeeded" || s.status === "failed") ok.add(s.step_id)
    if (isTerminalStep(s.status)) terminal.add(s.step_id)
  }
  // `success()` is transitive: a failure anywhere in the ancestry taints a step, even
  // through a succeeded `always()` step in between.
  for (let grew = tainted.size > 0; grew; ) {
    grew = false
    for (const s of steps)
      if (!tainted.has(s.step_id) && s.needs.some((n) => tainted.has(n))) {
        tainted.add(s.step_id)
        grew = true
      }
  }
  const plan: Transition[] = []
  for (const [index, s] of steps.entries()) {
    if (s.status !== "pending" && s.status !== "queued") continue
    const skip = (reason: string): void => {
      plan.push({ index, to: "skipped", reason })
    }
    const always = s.if !== null && trim(s.if) === "always()"
    if (!always && s.needs.some((n) => tainted.has(n))) {
      skip("dependency failed")
      continue
    }
    if (s.status === "queued" || !s.needs.every((n) => terminal.has(n))) continue
    const succeeded = s.needs.every((n) => ok.has(n) && !tainted.has(n))
    if (!always && !succeeded) skip("dependency skipped or failed")
    else if (!evalIf(s.if, succeeded, s.env)) skip("if: condition false")
    else plan.push({ index, to: "queued" })
  }
  return plan
}

/** Plan to a fixpoint. Returns each step's transition; the input is not modified. */
export function planToFixpoint(steps: readonly PlanStep[]): Transition[] {
  const cur = steps.map((s) => ({ ...s }))
  const all = new Map<number, Transition>()
  for (;;) {
    const plan = planTransitions(cur)
    if (plan.length === 0) return [...all.values()]
    for (const t of plan) {
      ;(cur[t.index] as PlanStep).status = t.to
      all.set(t.index, t)
    }
  }
}

/** The run's outcome once every step is done, or `null` while any is open. */
export function runOutcome(steps: readonly PlanStep[]): "succeeded" | "failed" | null {
  if (!steps.every((s) => isTerminalStep(s.status))) return null
  return steps.some(broke) ? "failed" : "succeeded"
}

export const MAX_RETRY_BACKOFF_SECS = 60

/** Backoff in seconds before another attempt, or `null`. Only a failure retries. */
export function retryBackoff(status: StepStatus, attempt: number, retries: number): number | null {
  if (status !== "failed" || attempt > retries) return null
  return Math.min(2 ** Math.max(attempt, 1), MAX_RETRY_BACKOFF_SECS)
}

/** A lost lease gets one try more than a reported failure: `retries + 2` leases in total. */
export const requeueAfterLostLease = (attempt: number, retries: number): boolean =>
  attempt <= retries + 1

/** Statuses a runner may report. Anything else in a completion is rejected at the boundary. */
export type ReportedStatus = "succeeded" | "failed" | "cancelled"
export const isReportedStatus = (s: unknown): s is ReportedStatus =>
  s === "succeeded" || s === "failed" || s === "cancelled"

/** Whether a report still describes the row: same attempt *and* same lease. */
export const reportIsCurrent = (
  row: { status: StepStatus; attempt: number; lease_id: string | null },
  report: { attempt: number; lease_id: string }
): boolean =>
  row.status === "running" && row.attempt === report.attempt && row.lease_id === report.lease_id

export const labelsMatch = (agent: readonly string[], required: readonly string[]): boolean =>
  required.every((r) => agent.includes(r))

/** `{pipeline}` is the id, since names change; an unknown placeholder stays literal. */
export const resolveConcurrencyGroup = (
  template: string | null,
  pipelineId: string,
  headRef: string | null
): string =>
  (template ?? "{pipeline}-{ref}")
    .replaceAll("{pipeline}", () => pipelineId)
    .replaceAll("{ref}", () => headRef ?? "")

export const cancelIsNoop = (run: RunStatus): boolean =>
  run === "succeeded" || run === "failed" || run === "cancelled"
