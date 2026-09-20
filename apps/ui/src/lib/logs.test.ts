import { describe, expect, it } from "vitest"
import {
  appendLogRows,
  highestLogId,
  type LogRow,
  MAX_LOG_ROWS,
  nextLogCursor,
  renderLogLine,
} from "./logs"

const row = (id: number, text = `line ${id}`): LogRow => ({ id, text })

describe("the log cursor", () => {
  it("is the last line of the page, which is what `after_id` means", () => {
    const page = [row(10), row(11), row(12)]
    expect(nextLogCursor(page, null)).toBe(12)
  })

  it("does not stall on a page that ends in a line carrying no id", () => {
    // A replica older than `log_batch` publishes lines without ids. Reading the cursor
    // off the last element would leave it where it was and refetch the same page.
    expect(nextLogCursor([row(10), row(11), row(-1)], 9)).toBe(11)
  })

  it("never regresses below what the viewer already holds", () => {
    expect(nextLogCursor([row(4), row(5)], 12)).toBe(12)
  })

  it("is left alone by an empty page, so a quiet step does not rewind", () => {
    expect(nextLogCursor([], 12)).toBe(12)
    expect(nextLogCursor([], null)).toBeNull()
  })

  it("matches the server's own cursor for a page as the server returns it", () => {
    // The 4a contract: a page is ordered by id and its last line is the next
    // `after_id`. Taking the maximum agrees with that for every page the server can
    // produce — and nothing here reorders one, so the append order is still arrival
    // order.
    const page = [row(10), row(11), row(12)]
    expect(nextLogCursor(page, null)).toBe(page[page.length - 1].id)
  })
})

describe("appending lines", () => {
  it("keeps the server's order", () => {
    const rows = appendLogRows([], [row(4), row(5), row(6)])
    expect(rows.map((r) => r.id)).toEqual([4, 5, 6])
  })

  it("drops what a live event already delivered when the catch-up page repeats it", () => {
    // After a resync the viewer refetches from the id it holds, and lines can arrive
    // both ways. Twice on screen is the bug this prevents.
    const live = appendLogRows([], [row(7), row(8)])
    const caughtUp = appendLogRows(live, [row(7), row(8), row(9)])
    expect(caughtUp.map((r) => r.id)).toEqual([7, 8, 9])
  })

  it("drops the overlap inside a single batch, which is how a catch-up commits", () => {
    // The real shape: the catch-up queues the fetched pages and then, in the same
    // frame, the live lines it held back — and those are a subset of what it fetched,
    // because the server stores a line before publishing it. Both reach `appendLogRows`
    // as one batch, so comparing only against the previous buffer prints the overlap
    // twice and leaves the buffer out of order.
    const held = appendLogRows([], [row(1000)])
    const fetched = [row(1001), row(1002), row(1003), row(1004), row(1005)]
    const heldBack = [row(1001), row(1002), row(1003), row(1004), row(1005)]
    const after = appendLogRows(held, [...fetched, ...heldBack])
    expect(after.map((r) => r.id)).toEqual([1000, 1001, 1002, 1003, 1004, 1005])
    // And the cursor the next resync starts from is still the buffer's true maximum.
    expect(highestLogId(after)).toBe(1005)
  })

  it("keeps a live line the catch-up page did not reach", () => {
    const held = appendLogRows([], [row(1000)])
    const after = appendLogRows(held, [
      row(1001),
      row(1002),
      row(1002),
      row(1003),
    ])
    expect(after.map((r) => r.id)).toEqual([1000, 1001, 1002, 1003])
  })

  it("never drops an id-less line from an older replica", () => {
    const held = appendLogRows([], [row(7)])
    const withLegacy = appendLogRows(held, [
      row(-1, "no id"),
      row(-2, "nor me"),
    ])
    expect(withLegacy.map((r) => r.text)).toEqual(["line 7", "no id", "nor me"])
  })

  it("caps the buffer from the tail", () => {
    const many = Array.from({ length: MAX_LOG_ROWS + 50 }, (_, i) => row(i + 1))
    const rows = appendLogRows([], many)
    expect(rows).toHaveLength(MAX_LOG_ROWS)
    expect(rows[0].id).toBe(51)
    expect(rows[rows.length - 1].id).toBe(MAX_LOG_ROWS + 50)
  })

  it("reports the true maximum even when the buffer ends out of order", () => {
    // Belt and braces for the case above: if anything ever does append behind the
    // tail, the cursor must still be the highest id held or the next resync refetches
    // lines already on screen.
    expect(highestLogId([row(9), row(11), row(10)])).toBe(11)
    expect(highestLogId([row(11), row(-1)])).toBe(11)
  })

  it("still knows its highest id after the cap has dropped lines", () => {
    const many = Array.from({ length: MAX_LOG_ROWS + 50 }, (_, i) => row(i + 1))
    const rows = appendLogRows([], many)
    expect(highestLogId(rows)).toBe(MAX_LOG_ROWS + 50)
    // And an id-less tail does not hide it.
    expect(highestLogId([...rows, row(-1)])).toBe(MAX_LOG_ROWS + 50)
    expect(highestLogId([row(-1)])).toBeNull()
  })
})

describe("rendering", () => {
  it("names the stream, defaulting to stdout", () => {
    expect(renderLogLine("stderr", "boom")).toBe("[stderr] boom")
    expect(renderLogLine(undefined, "hello")).toBe("[out] hello")
  })
})
