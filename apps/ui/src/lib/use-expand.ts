import { useCallback, useEffect, useState } from "react"

/**
 * Focus mode for a work area: the caller renders its region over the viewport while
 * `expanded` is set, and gets Escape and the page's scroll lock handled here.
 *
 * Deliberately not the browser's Fullscreen API. Real fullscreen takes the whole
 * screen but only for one element — the canvas would lose the inspector and the log
 * stream beside it, and clicking a step would lead nowhere. Covering the viewport
 * ourselves keeps the work area intact, behaves the same in every browser, and leaves
 * the exit affordance ours to draw.
 */
export function useExpand(options?: { escapeToExit?: boolean }) {
  const escapeToExit = options?.escapeToExit ?? true
  const [expanded, setExpanded] = useState(false)

  useEffect(() => {
    if (!expanded || !escapeToExit) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return
      e.preventDefault()
      // Escape is overloaded on these routes — it also clears the step selection. The
      // document sees the event before window does, so stopping it here keeps one
      // press from collapsing the canvas and deselecting in the same breath.
      e.stopPropagation()
      setExpanded(false)
    }
    document.addEventListener("keydown", onKey)
    return () => document.removeEventListener("keydown", onKey)
  }, [expanded, escapeToExit])

  useEffect(() => {
    if (!expanded) return
    const previous = document.body.style.overflow
    document.body.style.overflow = "hidden"
    return () => {
      document.body.style.overflow = previous
    }
  }, [expanded])

  const toggle = useCallback(() => setExpanded((v) => !v), [])

  return { expanded, setExpanded, toggle }
}
