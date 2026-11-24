/**
 * Six-field cron with seconds (`SEC MIN HOUR DAY MONTH DOW [YEAR]`), UTC.
 *
 * Day of week is 1 = Sunday to 7 = Saturday, and day-of-month and day-of-week must *both*
 * match. Whitespace between fields is optional wherever the grammar is unambiguous.
 */
type Spec =
  | { k: "all" }
  | { k: "point"; n: number }
  | { k: "range"; a: number; b: number }
  | { k: "named"; a: string; b: string }
type Root =
  | { k: "spec"; s: Spec }
  | { k: "period"; s: Spec; step: number }
  | { k: "name"; n: string }
interface Unit {
  min: number
  max: number
  names?: Record<string, number>
}
const MONTH_NAMES = [
  "january",
  "february",
  "march",
  "april",
  "may",
  "june",
  "july",
  "august",
  "september",
  "october",
  "november",
  "december",
]
const months: Record<string, number> = {}
MONTH_NAMES.forEach((m, i) => {
  months[m] = i + 1
  months[m.slice(0, 3)] = i + 1
})
const dow: Record<string, number> = {
  sun: 1,
  sunday: 1,
  mon: 2,
  monday: 2,
  tue: 3,
  tues: 3,
  tuesday: 3,
  wed: 4,
  wednesday: 4,
  thu: 5,
  thurs: 5,
  thursday: 5,
  fri: 6,
  friday: 6,
  sat: 7,
  saturday: 7,
}
const UNITS: Unit[] = [
  { min: 0, max: 59 },
  { min: 0, max: 59 },
  { min: 0, max: 23 },
  { min: 1, max: 31 },
  { min: 1, max: 12, names: months },
  { min: 1, max: 7, names: dow },
  { min: 1970, max: 2100 },
]
const SHORTHAND: Record<string, string> = {
  "@yearly": "0 0 0 1 1 *",
  "@monthly": "0 0 0 1 * *",
  "@weekly": "0 0 0 * * 1",
  "@daily": "0 0 0 * * *",
  "@hourly": "0 0 * * * *",
}
export type CronSchedule = Set<number>[]
export class CronError extends Error {}

export function parseCron(expr: string): CronSchedule {
  const short = /^[ \t\r\n]*(@[a-z]+)[ \t\r\n]*$/.exec(expr)
  const src = short ? SHORTHAND[short[1] as string] : expr
  if (src === undefined) throw new CronError(`unknown shorthand \`${expr.trim()}\``)
  let i = 0
  const ws = (): void => {
    while (i < src.length && " \t\r\n".includes(src[i] as string)) i++
  }
  /** Run `f`; on failure restore the position. */
  const attempt = <T>(f: () => T | null): T | null => {
    const at = i
    const r = f()
    if (r === null) i = at
    return r
  }
  const re = (r: RegExp): string | null => {
    r.lastIndex = i
    const m = r.exec(src)
    if (!m) return null
    i += m[0].length
    return m[0]
  }
  const eat = (c: string): true | null => {
    if (src[i] !== c) return null
    i++
    return true
  }
  const ordinal = (): number | null =>
    attempt(() => {
      ws()
      const d = re(/[0-9]+/y)
      if (d === null || Number(d) > 0xffffffff) return null
      ws()
      return Number(d)
    })
  const name = (): string | null =>
    attempt(() => {
      ws()
      const n = re(/[A-Za-z]+/y)
      if (n === null) return null
      ws()
      return n
    })
  const specifier = (any: boolean): Spec | null => {
    if (any && eat("?")) return { k: "all" }
    if (eat("*")) return { k: "all" }
    const range = attempt((): Spec | null => {
      const a = ordinal()
      if (a === null || !eat("-")) return null
      const b = ordinal()
      return b === null ? null : { k: "range", a, b }
    })
    if (range) return range
    const n = ordinal()
    if (n !== null) return { k: "point", n }
    return attempt((): Spec | null => {
      const a = name()
      if (a === null || !eat("-")) return null
      const b = name()
      return b === null ? null : { k: "named", a, b }
    })
  }
  const root = (any: boolean): Root | null => {
    const period = attempt((): Root | null => {
      const s = specifier(any)
      if (!s || !eat("/")) return null
      const step = ordinal()
      return step === null ? null : { k: "period", s, step }
    })
    if (period) return period
    const s = attempt(() => specifier(any))
    if (s) return { k: "spec", s }
    const n = name()
    return n === null ? null : { k: "name", n }
  }
  const field = (idx: number): Set<number> | null =>
    attempt(() => {
      const any = idx === 3 || idx === 5
      ws()
      const first = root(any)
      if (!first) return null
      const list = [first]
      for (;;) {
        const more = attempt(() => (eat(",") ? root(any) : null))
        if (!more) break
        list.push(more)
      }
      ws()
      const out = new Set<number>()
      for (const r of list) {
        const set = expand(r, UNITS[idx] as Unit)
        if (!set) return null
        for (const n of set) out.add(n)
      }
      return out
    })

  const fields: CronSchedule = []
  for (let f = 0; f < 6; f++) {
    const s = field(f)
    if (!s) throw new CronError(`field ${f + 1} is not valid at position ${i + 1}`)
    fields.push(s)
  }
  fields.push(field(6) ?? span(1970, 2100))
  if (i !== src.length) throw new CronError(`unexpected input at position ${i + 1}`)
  return fields
}

function span(a: number, b: number): Set<number> {
  const s = new Set<number>()
  for (let n = a; n <= b; n++) s.add(n)
  return s
}

function expand(r: Root, u: Unit): Set<number> | null {
  const ok = (n: number): boolean => n >= u.min && n <= u.max
  const named = (n: string): number | null => u.names?.[n.toLowerCase()] ?? null
  const spec = (s: Spec): Set<number> | null => {
    if (s.k === "all") return span(u.min, u.max)
    if (s.k === "point") return ok(s.n) ? new Set([s.n]) : null
    const a = s.k === "range" ? s.a : named(s.a)
    const b = s.k === "range" ? s.b : named(s.b)
    return a !== null && b !== null && ok(a) && ok(b) && a <= b ? span(a, b) : null
  }
  if (r.k === "name") {
    const n = named(r.n)
    return n === null ? null : new Set([n])
  }
  if (r.k === "spec") return spec(r.s)
  if (r.step < 1 || r.step > u.max) return null
  const base = r.s.k === "point" ? (ok(r.s.n) ? span(r.s.n, u.max) : null) : spec(r.s)
  return base && new Set([...base].filter((_, i) => i % r.step === 0))
}

/** The first occurrence strictly after `after` (epoch ms), or `null`. */
export function nextCron(expr: string, after: number): number | null {
  let f: CronSchedule
  try {
    f = parseCron(expr)
  } catch (e) {
    if (e instanceof CronError) return null
    throw e
  }
  const start = new Date((Math.floor(after / 1000) + 1) * 1000)
  const [sec, min, hour, dom, mon, wd, yr] = f.map((s) => [...s].sort((a, b) => a - b)) as [
    number[],
    number[],
    number[],
    number[],
    number[],
    number[],
    number[],
  ]
  const wdSet = new Set(wd)
  for (const y of yr) {
    if (y < start.getUTCFullYear()) continue
    for (const m of mon) {
      const days = new Date(Date.UTC(y, m, 0)).getUTCDate()
      if (Date.UTC(y, m - 1, days, 23, 59, 59) < start.getTime()) continue
      for (const d of dom) {
        if (d > days) break
        const day = Date.UTC(y, m - 1, d)
        if (!wdSet.has(new Date(day).getUTCDay() + 1)) continue
        if (day + 86_399_000 < start.getTime()) continue
        for (const h of hour) {
          if (day + h * 3_600_000 + 3_599_000 < start.getTime()) continue
          for (const mi of min) {
            const base = day + h * 3_600_000 + mi * 60_000
            if (base + 59_000 < start.getTime()) continue
            for (const s of sec) if (base + s * 1000 >= start.getTime()) return base + s * 1000
          }
        }
      }
    }
  }
  return null
}
