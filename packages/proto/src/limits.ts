import { byteLength } from "./text.ts"

/** The most UTF-8 bytes one log line may carry, marker included. */
export const MAX_LOG_LINE_BYTES = 64 * 1024
export const TRUNCATION_MARKER = "…[truncated]"
/** Bytes of a raw line to keep before decoding, leaving room for the marker. */
export const MAX_RAW_LINE_BYTES = MAX_LOG_LINE_BYTES - byteLength(TRUNCATION_MARKER)

export function truncateLogLine(s: string): string {
  if (s.length * 3 <= MAX_LOG_LINE_BYTES || byteLength(s) <= MAX_LOG_LINE_BYTES) return s
  let bytes = 0
  let i = 0
  for (const ch of s) {
    const n = byteLength(ch)
    if (bytes + n > MAX_RAW_LINE_BYTES) break
    bytes += n
    i += ch.length
  }
  return s.slice(0, i) + TRUNCATION_MARKER
}
