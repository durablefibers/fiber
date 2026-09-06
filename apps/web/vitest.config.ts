import viteReact from "@vitejs/plugin-react"
import { defineConfig } from "vitest/config"

// Separate from vite.config.ts on purpose: the TanStack Start plugin is a
// server/SSR build plugin and must not wrap unit tests.
export default defineConfig({
  resolve: { tsconfigPaths: true },
  plugins: [viteReact()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    setupFiles: ["./src/test-setup.ts"],
  },
})
