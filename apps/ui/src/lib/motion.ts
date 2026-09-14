/**
 * Whether this viewer has asked the system for less motion.
 *
 * Read at call time rather than cached: the setting can change while the tab is open,
 * and these are one-shot decisions (how long to animate a viewport move), not a
 * subscription. Returns false during SSR, where nothing is animating yet anyway.
 */
export function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return false
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches
}

/** A motion duration in milliseconds, or 0 when the viewer asked for less of it. */
export function motionDuration(ms: number): number {
  return prefersReducedMotion() ? 0 : ms
}
