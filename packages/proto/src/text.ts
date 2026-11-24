/**
 * Text rules shared with the original server, which were Rust's: Unicode `White_Space`
 * (JS `\s` adds U+FEFF and omits U+0085) and lengths counted in UTF-8 bytes.
 */
const WS = "\\t-\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000"
const LEADING = new RegExp(`^[${WS}]+`)
const TRAILING = new RegExp(`[${WS}]+$`)
const ANY_WS = new RegExp(`[${WS}]`)

export const trim = (s: string): string => s.replace(LEADING, "").replace(TRAILING, "")
export const hasWhitespace = (s: string): boolean => ANY_WS.test(s)
export const asciiLower = (s: string): string =>
  s.replace(/[A-Z]/g, (c) => String.fromCharCode(c.charCodeAt(0) + 32))
export const asciiUpper = (s: string): string =>
  s.replace(/[a-z]/g, (c) => String.fromCharCode(c.charCodeAt(0) - 32))

/** Unicode category Cc. */
export function hasControl(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c < 0x20 || (c >= 0x7f && c <= 0x9f)) return true
  }
  return false
}

/** Length in UTF-8 bytes, without allocating. */
export function byteLength(s: string): number {
  let n = 0
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c < 0x80) n += 1
    else if (c < 0x800) n += 2
    else if (c >= 0xd800 && c < 0xdc00 && i + 1 < s.length) {
      const d = s.charCodeAt(i + 1)
      if (d >= 0xdc00 && d < 0xe000) {
        n += 4
        i++
      } else n += 3
    } else n += 3
  }
  return n
}

/** Order strings by code point, as a byte-wise comparison of their UTF-8 would. */
export function compareCodePoints(a: string, b: string): number {
  const x = Array.from(a)
  const y = Array.from(b)
  for (let i = 0; i < x.length && i < y.length; i++) {
    const d =
      ((x[i] as string).codePointAt(0) as number) - ((y[i] as string).codePointAt(0) as number)
    if (d !== 0) return d
  }
  return x.length - y.length
}
