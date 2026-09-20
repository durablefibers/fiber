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

/** The highest id in `rows`, or null when none of them came from the server. */
export function highestLogId(rows: LogRow[]): number | null {
  for (let i = rows.length - 1; i >= 0; i--) {
    const id = rows[i].id
    if (id > 0) return id
  }
  return null
}

/**
 * Append `incoming` to `prev`, dropping what is already held and capping the total.
 *
 * Ids are strictly increasing per step, so anything at or below the highest id already
 * held has been seen — which is what makes a catch-up fetch after a resync safe to run
 * while live lines are still arriving.
 */
export function appendLogRows(
  prev: LogRow[],
  incoming: LogRow[],
  cap: number = MAX_LOG_ROWS
): LogRow[] {
  if (incoming.length === 0) return prev
  const held = highestLogId(prev)
  const fresh =
    held === null ? incoming : incoming.filter((r) => r.id <= 0 || r.id > held)
  if (fresh.length === 0) return prev
  const next = [...prev, ...fresh]
  return next.length > cap ? next.slice(next.length - cap) : next
}

/**
 * The cursor to pass as the next `after_id`.
 *
 * The **last** line of the page, not the largest id in it: the server orders a page by
 * `id`, and taking anything else — or reordering the page first — would either re-request
 * lines already held or skip the ones between. An empty page leaves the cursor alone.
 */
export function nextLogCursor(
  page: { id: number }[],
  current: number | null
): number | null {
  const last = page[page.length - 1]
  return last ? last.id : current
}
