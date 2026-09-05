/** Dev-only: break Vite SSR↔client console echo loops that abort the server. */
const NOISY =
  /\[React Flow\]|React Flow:|hydrated but some attributes|data-cursor-ref|\[Server\].*\[vite\].*console\.(warn|error)/

function shouldDrop(args: unknown[]): boolean {
  try {
    return NOISY.test(args.map(String).join(" "))
  } catch {
    return false
  }
}

if (import.meta.env.DEV) {
  for (const method of ["warn", "error"] as const) {
    const original = console[method].bind(console)
    console[method] = (...args: unknown[]) => {
      if (shouldDrop(args)) return
      original(...(args as Parameters<typeof console.warn>))
    }
  }
}
