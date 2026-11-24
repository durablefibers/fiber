import { type PipelineTriggers, trim } from "@fiber/proto"
import { CronError, nextCron, parseCron } from "./cron.ts"
import { validateGlobs } from "./path-filter.ts"

const cronOf = (on: PipelineTriggers): string | null => (on.cron && trim(on.cron)) || null
const intervalOf = (on: PipelineTriggers): number | null => on.interval_minutes || null

/** Everything in `on:` that decides whether and when a pipeline fires; message or `null`. */
export function validateTriggers(on: PipelineTriggers, now: number): string | null {
  const expr = cronOf(on)
  if (expr) {
    try {
      parseCron(expr)
    } catch (e) {
      if (!(e instanceof CronError)) throw e
      return `invalid cron \`${expr}\`: ${e.message}`
    }
    if (nextCron(expr, now) === null)
      return `cron \`${expr}\` has no next occurrence — it can never fire`
  }
  for (const k of ["push", "pull_request"] as const) {
    const t = on[k]
    if (!t) continue
    const bad =
      validateGlobs(`on.${k}.paths`, t.paths) ??
      validateGlobs(`on.${k}.paths_ignore`, t.paths_ignore)
    if (bad) return bad
  }
  return null
}

/** Next wake after `after`. Cron wins over `interval_minutes`. */
export function nextDue(on: PipelineTriggers, after: number): number | null {
  const expr = cronOf(on)
  if (expr) return nextCron(expr, after)
  const m = intervalOf(on)
  return m === null ? null : after + m * 60_000
}

/** First due time on save: an interval is due at once, a cron at its next tick. */
export function initialDue(on: PipelineTriggers | null, now: number): number | null {
  if (!on) return null
  const expr = cronOf(on)
  if (expr) return nextCron(expr, now)
  return intervalOf(on) === null ? null : now
}

/** Identity of the schedule: an edit that keeps it keeps the pending due time. */
export function scheduleKey(on: PipelineTriggers | null): string | null {
  if (!on) return null
  const expr = cronOf(on)
  if (expr) return `cron:${expr}`
  const m = intervalOf(on)
  return m === null ? null : `interval:${m}`
}

export function scheduleLabel(on: PipelineTriggers): string {
  const expr = cronOf(on)
  if (expr) return `schedule:cron:${expr}`
  const m = intervalOf(on)
  return m === null ? "schedule" : `schedule:${m}m`
}
