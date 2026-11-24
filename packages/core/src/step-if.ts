import { asciiLower, compareCodePoints, hasWhitespace, trim } from "@fiber/proto"

export type Env = readonly (readonly [string, string])[]
const SUPPORTED = "`success()`, `always()`, `never()`, or `<matrix|env>.<key> == '<value>'`"
const isCall = (e: string): boolean => e === "always()" || e === "never()" || e === "success()"
const isKey = (s: string): boolean => /^[A-Za-z0-9_-]+$/.test(s)
const normalise = (e: string | null): string | null => (e && trim(e)) || null

function splitEq(e: string): [string, string] | null {
  const i = e.indexOf("==")
  return i < 0 ? null : [e.slice(0, i), e.slice(i + 2)]
}
const quoted = (s: string): boolean =>
  s.length >= 2 &&
  ((s.startsWith("'") && s.endsWith("'")) || (s.startsWith('"') && s.endsWith('"')))
const unquote = (s: string): string => (quoted(s) ? s.slice(1, -1) : s)

/** The name an equality reads, without its `matrix.` / `env.` prefix. */
function referencedKey(left: string): string | null {
  const l = trim(left)
  if (l === "") return null
  if (l.startsWith("matrix.")) return l.slice(7)
  return l.startsWith("env.") ? l.slice(4) : l
}

/** Shared by evaluation and `validateKey`, so the two can never disagree. */
function lookup(env: Env, key: string): string | null {
  const m = asciiLower(`MATRIX_${key}`)
  const f = asciiLower(`FIBER_MATRIX_${key}`)
  for (const [k, v] of env) {
    const lk = asciiLower(k)
    if (k === key || lk === m || lk === f) return v
  }
  return null
}

/** Evaluate a step condition; missing means `success()`. Not understood means false. */
export function evalIf(raw: string | null, needsSucceeded: boolean, env: Env): boolean {
  const e = normalise(raw) ?? "success()"
  if (e === "always()") return true
  if (e === "never()") return false
  if (e === "success()") return needsSucceeded
  const eq = splitEq(e)
  const key = eq && referencedKey(eq[0])
  if (!eq || key === null) return false
  const v = lookup(env, key)
  return v !== null && v === unquote(trim(eq[1]))
}

/** Reject an `if:` that cannot be evaluated; the message, or `null` when fine. */
export function validateIf(raw: string | null): string | null {
  const e = normalise(raw)
  if (e === null) return null
  const bad = (m: string): string => `if: \`${e}\` — ${m}`
  if (e.includes("${{"))
    return bad(
      `drop the \`\${{ }}\` wrapper; a Fiber condition is a bare expression (${SUPPORTED})`
    )
  for (const op of ["!=", "&&", "||", ">=", "<=", "=~"])
    if (e.includes(op)) return bad(`the \`${op}\` operator is not supported; use ${SUPPORTED}`)
  if (isCall(e)) return null
  const call = e.endsWith(")") ? e.indexOf("(") : -1
  if (call >= 0 && /^[A-Za-z0-9_]+$/.test(e.slice(0, call)))
    return bad(`unknown function \`${e.slice(0, call)}()\`; supported: ${SUPPORTED}`)
  const eq = splitEq(e)
  if (!eq) return bad(`not a condition this server understands; use ${SUPPORTED}`)
  if (eq[1].includes("=="))
    return bad("more than one `==`; a condition compares exactly two values")
  const left = trim(eq[0])
  if (left === "") return bad("nothing on the left of `==`")
  const dot = left.indexOf(".")
  if (dot >= 0) {
    const ctx = left.slice(0, dot)
    const key = left.slice(dot + 1)
    if (ctx !== "matrix" && ctx !== "env")
      return bad(
        `unknown context \`${ctx}\`; only \`matrix.\` and \`env.\` are available to a step condition`
      )
    if (!isKey(key)) return bad(`\`${ctx}.${key}\` is not a usable ${ctx} name`)
  } else if (!isKey(left))
    return bad(
      `\`${left}\` is not a matrix or env name; write \`matrix.${left} == '...'\` if that is what you meant`
    )
  const right = trim(eq[1])
  if (right === "") return bad("nothing on the right of `==`")
  if (!quoted(right)) {
    if (/['"]/.test(right)) return bad(`the value \`${right}\` has an unbalanced quote`)
    if (hasWhitespace(right)) return bad(`quote the value: \`${left} == '${right}'\``)
  }
  return null
}

/** Reject a condition reading a name this step does not have. */
export function validateKey(raw: string | null, env: Env): string | null {
  const e = normalise(raw)
  if (e === null || isCall(e)) return null
  const eq = splitEq(e)
  const key = eq && referencedKey(eq[0])
  if (!eq || key === null || lookup(env, key) !== null) return null
  const names = [...new Set(env.map(([k]) => k))]
    .filter((k) => !k.startsWith("MATRIX_") && !k.startsWith("FIBER_MATRIX_"))
    .sort(compareCodePoints)
  const avail = names.length
    ? `; available: ${names.join(", ")}`
    : "; this step has no matrix axes and no env"
  return `if: \`${e}\` — \`${key}\` is not a matrix axis or an env name on this step${avail}`
}
