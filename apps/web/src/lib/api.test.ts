import { afterEach, describe, expect, it, vi } from "vitest"
import {
  api,
  definitionToYaml,
  type PipelineDefinition,
  setToken,
  statusColor,
} from "./api"

const def: PipelineDefinition = {
  name: "build and test",
  workspace: { repo: "https://github.com/org/repo.git", ref: "main" },
  on: {
    push: { branches: ["main"], paths: ["src/**"], paths_ignore: ["*.md"] },
    pull_request: { branches: ["main"], types: ["opened"] },
    cron: "0 */15 * * * *",
  },
  timeout_minutes: 90,
  steps: [
    {
      id: "checkout",
      name: "checkout",
      needs: [],
      run: "git status",
      labels: ["os=linux"],
    },
    {
      id: "test",
      name: "Run tests",
      needs: ["checkout"],
      run: "make test\necho done",
      image: "rust:1.97",
      retries: 2,
      timeout_minutes: 15,
      if: "matrix.os == 'linux'",
      matrix: { os: ["linux", "macos"] },
      artifacts: ["out/report.xml"],
    },
  ],
}

describe("definitionToYaml", () => {
  it("emits the fiber.yml shape the server parses", () => {
    const yaml = definitionToYaml(def)
    expect(yaml).toBe(
      [
        'name: "build and test"',
        "workspace:",
        "  repo: https://github.com/org/repo.git",
        "  ref: main",
        "on:",
        "  push:",
        "    branches: [main]",
        '    paths: ["src/**"]',
        '    paths_ignore: ["*.md"]',
        "  pull_request:",
        "    branches: [main]",
        "    types: [opened]",
        '  cron: "0 */15 * * * *"',
        "timeout_minutes: 90",
        "steps:",
        "  checkout:",
        '    labels: ["os=linux"]',
        '    run: "git status"',
        "  test:",
        '    name: "Run tests"',
        "    needs: [checkout]",
        "    image: rust:1.97",
        "    retries: 2",
        "    timeout_minutes: 15",
        "    if: \"matrix.os == 'linux'\"",
        "    matrix:",
        "      os: [linux, macos]",
        "    artifacts: [out/report.xml]",
        "    run: |",
        "      make test",
        "      echo done",
        "",
      ].join("\n")
    )
  })

  it("omits empty sections", () => {
    const yaml = definitionToYaml({
      name: "min",
      steps: [{ id: "a", name: "a", needs: [], run: "true" }],
    })
    expect(yaml).toBe("name: min\nsteps:\n  a:\n    run: true\n")
    expect(yaml).not.toContain("on:")
    expect(yaml).not.toContain("workspace:")
  })
})

describe("statusColor", () => {
  it("maps every known status to a class", () => {
    for (const s of [
      "pending",
      "queued",
      "running",
      "succeeded",
      "failed",
      "cancelled",
      "skipped",
    ]) {
      expect(statusColor(s)).toBeTruthy()
    }
  })
})

describe("request error handling", () => {
  afterEach(() => {
    vi.unstubAllGlobals()
    setToken(null)
  })

  it("surfaces the server error message, not the JSON envelope", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"forbidden"}', { status: 403 }))
    )
    await expect(api.listProjects()).rejects.toThrow(/^forbidden$/)
  })

  it("falls back to the raw body when it is not an envelope", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("bad gateway", { status: 502 }))
    )
    await expect(api.listProjects()).rejects.toThrow(/^bad gateway$/)
  })

  it("sends the bearer token when one is stored", async () => {
    setToken("fiber_sess_test")
    const fetchMock = vi.fn<typeof fetch>(async () => Response.json([]))
    vi.stubGlobal("fetch", fetchMock)
    await api.listProjects()
    const headers = fetchMock.mock.calls[0]?.[1]?.headers as
      | Record<string, string>
      | undefined
    expect(headers?.Authorization).toBe("Bearer fiber_sess_test")
  })
})
