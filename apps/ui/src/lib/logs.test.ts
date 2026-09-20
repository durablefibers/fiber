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

  it("is left alone by an empty page, so a quiet step does not rewind", () => {
    expect(nextLogCursor([], 12)).toBe(12)
    expect(nextLogCursor([], null)).toBeNull()
  })

  it("reads the page in the order the server sent it", () => {
    // The server orders by id; taking the position rather than the value is what keeps
    // the next request contiguous. A page reordered client-side would make this wrong,
    // so nothing here may sort.
    const page = [row(10), row(11), row(12)]
    const reordered = [...page].reverse()
    expect(nextLogCursor(page, null)).toBe(12)
    expect(nextLogCursor(reordered, null)).toBe(10)
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
