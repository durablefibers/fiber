import { trim } from "@fiber/proto"
import { compileGlob, GlobError, parseGlob } from "./glob.ts"

const utf8 = new TextEncoder()

/** The first unusable pattern as a message, or `null`. Used on save. */
export function validateGlobs(field: string, patterns: readonly string[]): string | null {
  for (const p of patterns) {
    const g = trim(p)
    if (g === "") return `${field}: an empty pattern matches nothing; remove it`
    try {
      parseGlob(g)
    } catch (e) {
      if (!(e instanceof GlobError)) throw e
      return `${field}: invalid glob \`${g}\`: ${e.message}`
    }
  }
  return null
}

/** Lenient: a stored definition that predates validation still has to evaluate. */
function compileSet(patterns: readonly string[]): (path: string) => boolean {
  const ms: ((b: Uint8Array) => boolean)[] = []
  for (const p of patterns) {
    const g = trim(p)
    if (g === "") continue
    try {
      ms.push(compileGlob(g))
    } catch (e) {
      if (!(e instanceof GlobError)) throw e
    }
  }
  return (path) => {
    const b = utf8.encode(path)
    return ms.some((m) => m(b))
  }
}

/** Whether a webhook touching `changed` files fires under these filters. */
export function pathsAllow(
  changed: readonly string[],
  paths: readonly string[],
  ignore: readonly string[]
): boolean {
  if (paths.length === 0 && ignore.length === 0) return true
  if (changed.length === 0) return false
  const inc = compileSet(paths)
  const exc = compileSet(ignore)
  return changed.some((f) => !exc(f) && (paths.length === 0 || inc(f)))
}
