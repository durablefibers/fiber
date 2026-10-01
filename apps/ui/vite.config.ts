import tailwindcss from "@tailwindcss/vite"
import { devtools } from "@tanstack/devtools-vite"
import { tanstackStart } from "@tanstack/react-start/plugin/vite"
import viteReact from "@vitejs/plugin-react"
import { defineConfig } from "vite"

const config = defineConfig({
  resolve: { tsconfigPaths: true },
  plugins: [
    devtools(),
    tailwindcss(),
    // SPA mode: the image is static files behind nginx, so the build has to write the app
    // shell as index.html — nginx's fallback for every client-side route. Without it the
    // build emitted no HTML at all, and the container served nginx's own welcome page.
    tanstackStart({
      spa: { enabled: true, prerender: { outputPath: "/index" } },
    }),
    viteReact(),
  ],
})

export default config
