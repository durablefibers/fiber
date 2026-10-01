import { defineConfig, devices } from "@playwright/test"

// One browser flow against a running stack: `scripts/smoke_compose.sh` runs it while the
// Compose stack and its agent are up. Not part of `pnpm test` (vitest), which needs no
// stack at all.
export default defineConfig({
  testDir: "e2e",
  timeout: 120_000,
  retries: 0,
  reporter: "list",
  use: {
    // localhost, not 127.0.0.1: the UI image calls http://localhost:18080, and the API's
    // CORS allowlist pairs each origin with that spelling.
    baseURL: process.env.FIBER_E2E_UI ?? "http://localhost:3100",
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
})
