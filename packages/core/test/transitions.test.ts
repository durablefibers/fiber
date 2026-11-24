import { readFileSync } from "node:fs"
import { expect, test } from "vitest"
import {
  isReportedStatus,
  type PlanStep,
  planToFixpoint,
  planTransitions,
  reportIsCurrent,
  requeueAfterLostLease,
  resolveConcurrencyGroup,
  retryBackoff,
  runOutcome,
} from "../src/index.ts"

interface Case {
  steps: { step_id: string; status: PlanStep["status"]; needs: string[] }[]
  snapshot: Pick<PlanStep, "if" | "env" | "continue_on_error">[]
  first: unknown[]
  end: string[]
}
/** Emitted by `store::propagate_tests::emit_transition_vectors`. */
const cases: Case[] = JSON.parse(
  readFileSync(new URL("../../../conformance/vectors/transitions.json", import.meta.url), "utf8")
)

test("one pass and the fixpoint match the oracle on random runs", () => {
  expect(cases.length).toBe(2000)
  for (const c of cases) {
    const steps = c.steps.map((s, i) => ({ ...c.snapshot[i], ...s })) as PlanStep[]
    const first = planTransitions(steps).map((t) =>
      t.to === "queued" ? [t.index, "queue"] : [t.index, "skip", t.reason]
    )
    expect(first).toEqual(c.first)
    const end: string[] = steps.map((s) => s.status)
    for (const t of planToFixpoint(steps)) end[t.index] = t.to
    expect(end).toEqual(c.end)
  }
})

const step = (status: PlanStep["status"], tolerated = false): PlanStep => ({
  step_id: "s",
  status,
  needs: [],
  if: null,
  env: [],
  continue_on_error: tolerated,
})

test("a run fails on an untolerated failure or any cancel, and is open while a step is", () => {
  expect(runOutcome([step("succeeded"), step("skipped")])).toBe("succeeded")
  expect(runOutcome([step("failed", true)])).toBe("succeeded")
  expect(runOutcome([step("failed")])).toBe("failed")
  expect(runOutcome([step("cancelled", true)])).toBe("failed")
  for (const open of ["pending", "queued", "running"] as const)
    expect(runOutcome([step("failed"), step(open)])).toBeNull()
})

test("retry budgets: reported failures stop at retries + 1, lost leases at retries + 2", () => {
  expect(retryBackoff("failed", 1, 0)).toBeNull()
  expect([1, 2, 5, 6, 64].map((a) => retryBackoff("failed", a, 99))).toEqual([2, 4, 32, 60, 60])
  expect(retryBackoff("failed", 2, 2)).toBe(4)
  expect(retryBackoff("failed", 3, 2)).toBeNull()
  expect(retryBackoff("cancelled", 1, 5)).toBeNull()
  expect(requeueAfterLostLease(1, 0)).toBe(true)
  expect(requeueAfterLostLease(2, 0)).toBe(false)
})

test("a report applies only to the running attempt and lease it names", () => {
  const row = { status: "running", attempt: 2, lease_id: "l2" } as const
  expect(reportIsCurrent(row, { attempt: 2, lease_id: "l2" })).toBe(true)
  expect(reportIsCurrent(row, { attempt: 1, lease_id: "l2" })).toBe(false)
  expect(reportIsCurrent(row, { attempt: 2, lease_id: "l1" })).toBe(false)
  expect(reportIsCurrent({ ...row, status: "cancelled" }, { attempt: 2, lease_id: "l2" })).toBe(
    false
  )
  for (const bad of ["pending", "queued", "running", "skipped", 1, null])
    expect(isReportedStatus(bad)).toBe(false)
})

test("concurrency groups expand literally", () => {
  expect(resolveConcurrencyGroup(null, "id", null)).toBe("id-")
  expect(resolveConcurrencyGroup("{ref}/{version}", "id", "$&")).toBe("$&/{version}")
})
