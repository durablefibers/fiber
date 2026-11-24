import { expect, test } from "vitest"
import {
  auditDefinition,
  compileDefinition,
  DagError,
  initialDue,
  MAX_RETRIES,
  MAX_TIMEOUT_MINUTES,
  maxStepsFrom,
  nextDue,
  parsePipelineYaml,
  parseYaml,
  scheduleKey,
  validateTriggers,
} from "../src/index.ts"

const on = (o: object) => ({
  push: null,
  pull_request: null,
  interval_minutes: null,
  cron: null,
  ...o,
})

test("a JSON submission is audited like YAML, in both shapes", () => {
  expect(auditDefinition({ name: "p", steps: [{ id: "a", "continue-on-error": true }] })).toBe(
    "step `a`: unknown field `continue-on-error` — did you mean `continue_on_error`?"
  )
  expect(auditDefinition({ yaml: "name: p\nsteps: {a: {run: x, need: []}}" })).toContain("`needs`")
  expect(auditDefinition({ name: "p", steps: [] })).toBeNull()
})

test("timeouts and retries are bounded at compile", () => {
  const def = (f: string) => () =>
    compileDefinition(parsePipelineYaml(`name: p\nsteps: {a: {run: x, ${f}}}`))
  expect(def(`timeout_minutes: ${MAX_TIMEOUT_MINUTES}`)).not.toThrow()
  expect(def(`timeout_minutes: ${MAX_TIMEOUT_MINUTES + 1}`)).toThrow(DagError)
  expect(def("timeout_minutes: 3000000000")).toThrow(DagError)
  expect(def(`retries: ${MAX_RETRIES + 1}`)).toThrow(DagError)
})

test("expanding aliases is bounded", () => {
  let y = "a0: &a0 [x, x, x, x, x, x, x, x, x]\n"
  for (let i = 1; i < 9; i++)
    y += `a${i}: &a${i} [${Array(9)
      .fill(`*a${i - 1}`)
      .join(", ")}]\n`
  expect(() => parseYaml(y)).toThrow()
})

test("a step cap that is not a positive number is refused", () => {
  expect(maxStepsFrom(" ")).toBe(500)
  expect(maxStepsFrom("2000")).toBe(2000)
  for (const bad of ["0", "x", "-1", "1.5"]) expect(() => maxStepsFrom(bad)).toThrow()
})

test("cron wins over an interval, and an interval is first due at once", () => {
  const t = Date.UTC(2026, 0, 1, 0, 0, 30)
  expect(initialDue(on({ interval_minutes: 5 }), t)).toBe(t)
  expect(nextDue(on({ interval_minutes: 5 }), t)).toBe(t + 300_000)
  const both = on({ interval_minutes: 5, cron: " 0 0 * * * * " })
  expect(nextDue(both, t)).toBe(Date.UTC(2026, 0, 1, 1))
  expect(scheduleKey(both)).toBe("cron:0 0 * * * *")
  expect(scheduleKey(on({ interval_minutes: 0 }))).toBeNull()
  expect(validateTriggers(on({ cron: "0 0 0 31 2 *" }), t)).toContain("can never fire")
})
