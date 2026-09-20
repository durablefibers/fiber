/**
 * The run page's log buffer.
 *
 * Lines reach the viewer two ways — live over `/ws/runs/{id}`, and by refetching after a
 * gap — and the two overlap. `log_lines.id` is what makes that safe: the server returns a
 * page ordered by `id` and the last line of a page is the cursor for the next `after_id`,
 * so a viewer that remembers the highest id it holds can ask for exactly what it missed
 * and recognise anything it already has.
 */

/** One line as shown, with the server id it came from. */
export type LogRow = {
  /** `log_lines.id`. Negative for a line from an older replica's single-line `log`
   * event, which carries no id: those are never deduplicated, only kept unique. */
  id: number
  text: string
}

/** Lines kept in the DOM's model for one attempt. Older ones are dropped from the tail. */
export const MAX_LOG_ROWS = 800

export function renderLogLine(
  stream: string | undefined,
  data: string
): string {
  return `[${stream ?? "out"}] ${data}`
}

/**
 * The highest id in `rows`, or null when none of them came from the server.
 *
 * A real maximum, not the last positive row: the buffer is normally ordered, but a
 * catch-up appends a page behind lines that arrived live, and reading the tail would
 * then report a cursor lower than what is actually held — which re-appends the same
 * lines on the next resync.
 */
export function highestLogId(rows: { id: number }[]): number | null {
  let max: number | null = null
  for (const row of rows) {
    if (row.id > 0 && (max === null || row.id > max)) max = row.id
  }
  return max
}

/**
 * Append `incoming` to `prev`, dropping what is already held and capping the total.
 *
 * Ids are strictly increasing per step, so anything at or below the highest id already
 * held has been seen. The high-water mark advances **within** `incoming` as well, not
 * just against `prev`: a catch-up commits the fetched pages and the live lines it held
 * back in the same batch, and the held ones are by construction a subset of the fetched
 * ones — the server stores a line before it publishes it. Comparing only against `prev`
 * printed that overlap twice and left the buffer out of order, which made the next
 * resync worse than the one before.
 */
export function appendLogRows(
  prev: LogRow[],
  incoming: LogRow[],
  cap: number = MAX_LOG_ROWS
): LogRow[] {
  if (incoming.length === 0) return prev
  let held = highestLogId(prev)
  const fresh: LogRow[] = []
  for (const row of incoming) {
    // A line from a replica too old to send ids cannot be recognised, so it is always
    // kept; it also must not move the mark.
    if (row.id <= 0) {
      fresh.push(row)
      continue
    }
    if (held !== null && row.id <= held) continue
    fresh.push(row)
    held = row.id
  }
  if (fresh.length === 0) return prev
  const next = [...prev, ...fresh]
  return next.length > cap ? next.slice(next.length - cap) : next
}

/**
 * The cursor to pass as the next `after_id`.
 *
 * The highest id in the page. For a page as the server returns it — ordered by `id`,
 * contiguous, its last line the cursor for the next request — that *is* the last line,
 * so the 4a contract holds; taking the maximum rather than the position is what keeps it
 * holding when the page ends in a line that carries no id, and what stops the cursor
 * regressing below what the buffer already holds. Nothing here reorders a page: the
 * order lines are appended in is the order they arrived.
 *
 * An empty page, or one with no server ids in it, leaves the cursor alone.
 */
export function nextLogCursor(
  page: { id: number }[],
  current: number | null
): number | null {
  const highest = highestLogId(page)
  if (highest === null) return current
  return current === null ? highest : Math.max(current, highest)
}
