/**
 * Grammar for definition fields that become arguments to a process on a runner host.
 *
 * Checked when a pipeline compiles, so the author gets a legible error, and again on the
 * runner right before use. The runner is the boundary; the compile-time check is a courtesy.
 */
import { asciiLower, byteLength, hasControl, hasWhitespace, trim } from "./text.ts"

/** `[registry[:port]/][path/]name[:tag][@algo:hex]`: never readable as a docker flag. */
export function imageReferenceOk(s: string): boolean {
  if (s === "" || byteLength(s) > 255) return false
  const at = s.indexOf("@")
  const name = at < 0 ? s : s.slice(0, at)
  if (at >= 0 && !/^[A-Za-z0-9]+:[0-9A-Fa-f]{32,}$/.test(s.slice(at + 1))) return false
  if (!/^[A-Za-z0-9][A-Za-z0-9._\-/:]*$/.test(name)) return false
  if (name.includes("//") || name.endsWith("/") || name.endsWith(":")) return false
  const last = name.slice(name.lastIndexOf("/") + 1)
  return last !== "" && last.split(":").length <= 2
}

/** An http(s), ssh, git or file URL, scp-like `user@host:path`, or a path. */
export function repoUrlOk(raw: string): boolean {
  const s = trim(raw)
  if (s === "" || byteLength(s) > 2048 || s.startsWith("-")) return false
  if (hasWhitespace(s) || hasControl(s)) return false
  // `<helper>::<address>` at the very start invokes a git remote helper.
  if (/^[A-Za-z0-9+.-]*::/.test(s)) return false
  const sep = s.indexOf("://")
  let authority: string
  if (sep >= 0) {
    const rest = s.slice(sep + 3)
    const scheme = asciiLower(s.slice(0, sep))
    if (rest === "" || !["http", "https", "ssh", "git", "file"].includes(scheme)) return false
    authority = rest.split("/")[0] ?? ""
  } else {
    authority = s.split("/")[0] ?? ""
  }
  // A user or host beginning with `-` would be an ssh option.
  return !authority.split(/[@:]/).some((p) => p.startsWith("-"))
}

/** A full commit id: 40 or 64 hex characters. */
export const commitShaOk = (s: string): boolean =>
  (s.length === 40 || s.length === 64) && /^[0-9A-Fa-f]+$/.test(s)

export const gitRefOk = (r: string): boolean =>
  r !== "" &&
  byteLength(r) <= 255 &&
  !r.startsWith("-") &&
  !r.includes("..") &&
  !hasWhitespace(r) &&
  !hasControl(r) &&
  !/[~^:?]/.test(r)

export function workingDirectoryOk(raw: string): boolean {
  const p = trim(raw)
  return p !== "" && !/^[/\\]/.test(p) && !p.includes(":") && !p.split(/[/\\]/).includes("..")
}

/** A bare program name, never a path or a command line. */
export function shellOk(raw: string): boolean {
  const s = trim(raw)
  return s.length <= 32 && /^[A-Za-z0-9_.][A-Za-z0-9_.-]*$/.test(s) && s !== "." && s !== ".."
}

/** A workspace-relative path made only of plain components. */
export function artifactPathOk(s: string): boolean {
  if (s === "" || byteLength(s) > 1024 || hasControl(s) || s.startsWith("/")) return false
  const parts = s.split("/")
  // An interior `.` or an empty component is normalised away; a leading one is not.
  if (parts[0] === ".") return false
  const real = parts.filter((p) => p !== "" && p !== ".")
  return real.length > 0 && !real.includes("..")
}
