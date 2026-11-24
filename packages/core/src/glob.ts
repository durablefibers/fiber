/**
 * Glob matching with the semantics pipelines were written against: `*` and `?` cross `/`,
 * `**` collapses at separators, `{a,b}` nests, `\` escapes, and everything is matched over
 * UTF-8 bytes. Patterns come from users, so matching simulates an NFA in linear time
 * instead of handing a generated pattern to a backtracking RegExp.
 */
type Range = [string, string]
type Token =
  | { t: "lit"; c: string }
  | { t: "any" | "star" | "rprefix" | "rsuffix" | "rmid" }
  | { t: "class"; neg: boolean; ranges: Range[] }
  | { t: "alt"; branches: Token[][] }

export class GlobError extends Error {}
const utf8 = new TextEncoder()

export function parseGlob(glob: string): Token[] {
  const cs = Array.from(glob)
  let i = 0
  let prev: string | undefined
  let cur: string | undefined
  const bump = (): string | undefined => {
    prev = cur
    cur = cs[i]
    if (i < cs.length) i++
    return cur
  }
  const branches: Token[][] = [[]]
  const stack: number[] = []
  const last = (): Token[] => branches[branches.length - 1] as Token[]

  const parseStar = (): void => {
    const before = prev
    const twoStars = (): void => {
      last().push({ t: "star" }, { t: "star" })
    }
    if (cs[i] !== "*") {
      last().push({ t: "star" })
      return
    }
    bump()
    const next = cs[i]
    if (last().length === 0) {
      if (next !== undefined && next !== "/") twoStars()
      else {
        last().push({ t: "rprefix" })
        bump()
      }
      return
    }
    if (before !== "/" && (branches.length <= 1 || (before !== "," && before !== "{"))) {
      twoStars()
      return
    }
    let suffix: boolean
    if (next === undefined) {
      bump()
      suffix = true
    } else if ((next === "," || next === "}") && branches.length >= 2) suffix = true
    else if (next === "/") {
      bump()
      suffix = false
    } else {
      twoStars()
      return
    }
    const popped = last().pop()
    if (popped?.t === "rprefix" || popped?.t === "rsuffix") last().push(popped)
    else last().push({ t: suffix ? "rsuffix" : "rmid" })
  }

  const parseClass = (): void => {
    const ranges: Range[] = []
    let neg = false
    if (cs[i] === "!" || cs[i] === "^") {
      bump()
      neg = true
    }
    let first = true
    let inRange = false
    const close = (c: string): void => {
      const r = ranges[ranges.length - 1] as Range
      r[1] = c
      if ((r[1].codePointAt(0) as number) < (r[0].codePointAt(0) as number))
        throw new GlobError(`invalid range; '${r[0]}' > '${r[1]}'`)
    }
    for (;;) {
      const c = bump()
      if (c === undefined) throw new GlobError("unclosed character class; missing ']'")
      if (c === "]" && !first) break
      if (c === "-" && !first) {
        if (inRange) {
          close("-")
          inRange = false
        } else inRange = true
      } else if (inRange) {
        close(c)
        inRange = false
      } else ranges.push([c, c])
      first = false
    }
    if (inRange) ranges.push(["-", "-"])
    last().push({ t: "class", neg, ranges })
  }

  for (let c = bump(); c !== undefined; c = bump()) {
    if (c === "?") last().push({ t: "any" })
    else if (c === "*") parseStar()
    else if (c === "[") parseClass()
    else if (c === "{") {
      stack.push(branches.length)
      branches.push([])
    } else if (c === "}") {
      const start = stack.pop()
      if (start === undefined)
        throw new GlobError("unopened alternate group; missing '{' (maybe escape '}' with '[}]'?)")
      const alt = branches.splice(start)
      last().push({ t: "alt", branches: alt })
    } else if (c === "," && stack.length > 0) branches.push([])
    else if (c === "\\") {
      const e = bump()
      if (e === undefined) throw new GlobError("dangling '\\'")
      last().push({ t: "lit", c: e })
    } else last().push({ t: "lit", c })
  }
  if (branches.length > 1)
    throw new GlobError("unclosed alternate group; missing '}' (maybe escape '{' with '[{]'?)")
  return branches[0] as Token[]
}

// NFA: each state is an epsilon fork or a byte test leading to one state.
type State = { eps: number[]; test?: (b: number) => boolean; to?: number }

/** Compile to a predicate over UTF-8 bytes. Throws `GlobError` on a bad pattern. */
export function compileGlob(glob: string): (bytes: Uint8Array) => boolean {
  const tokens = parseGlob(glob)
  const st: State[] = []
  const add = (): number => st.push({ eps: [] }) - 1
  const link = (a: number, b: number): void => {
    ;(st[a] as State).eps.push(b)
  }
  const byte = (from: number, test: (b: number) => boolean): number => {
    const s = add()
    const to = add()
    link(from, s)
    ;(st[s] as State).test = test
    ;(st[s] as State).to = to
    return to
  }
  const slash = (b: number): boolean => b === 0x2f
  const anyByte = (): boolean => true
  const star = (from: number): number => {
    const loop = add()
    link(from, loop)
    ;(st[loop] as State).test = anyByte
    ;(st[loop] as State).to = loop
    const out = add()
    link(loop, out)
    return out
  }
  const join = (...ends: number[]): number => {
    const out = add()
    for (const e of ends) link(e, out)
    return out
  }
  const build = (ts: Token[], from: number): number => {
    let at = from
    for (const t of ts) {
      if (t.t === "lit") for (const b of utf8.encode(t.c)) at = byte(at, (x) => x === b)
      else if (t.t === "any") at = byte(at, anyByte)
      else if (t.t === "star") at = star(at)
      else if (t.t === "rprefix") at = join(at, byte(at, slash), byte(star(at), slash))
      else if (t.t === "rsuffix") at = star(byte(at, slash))
      else if (t.t === "rmid") {
        const one = byte(at, slash)
        at = join(one, byte(star(one), slash))
      } else if (t.t === "class") {
        const set = new Uint8Array(256)
        for (const [lo, hi] of t.ranges) {
          const a = utf8.encode(lo)
          const z = utf8.encode(hi)
          for (const b of a) set[b] = 1
          if (lo === hi) continue
          for (const b of z) set[b] = 1
          const from2 = a[a.length - 1] as number
          const to2 = z[0] as number
          if (from2 > to2) throw new GlobError("invalid character class range")
          for (let b = from2; b <= to2; b++) set[b] = 1
        }
        at = byte(at, (b) => (set[b] === 1) !== t.neg)
      } else if (t.t === "alt") {
        // An empty alternate is dropped unless every one is empty.
        const real = t.branches.filter((b) => b.length > 0)
        if (real.length > 0) {
          const start = at
          at = join(...real.map((b) => build(b, start)))
        }
      }
    }
    return at
  }
  const start = add()
  const only = tokens[0]
  const end = tokens.length === 1 && only?.t === "rprefix" ? star(start) : build(tokens, start)

  return (bytes) => {
    let cur = new Set<number>()
    const close = (set: Set<number>, s: number): void => {
      const todo = [s]
      for (let n = todo.pop(); n !== undefined; n = todo.pop()) {
        if (set.has(n)) continue
        set.add(n)
        todo.push(...(st[n] as State).eps)
      }
    }
    close(cur, start)
    for (const b of bytes) {
      const next = new Set<number>()
      for (const n of cur) {
        const s = st[n] as State
        if (s.test?.(b)) close(next, s.to as number)
      }
      if (next.size === 0) return false
      cur = next
    }
    return cur.has(end)
  }
}
