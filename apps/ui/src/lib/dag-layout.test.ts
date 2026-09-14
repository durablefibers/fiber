import { describe, expect, it } from "vitest"
import type { PipelineDefinition, StepDefinition } from "@/lib/api"
import {
  COL_WIDTH,
  layout,
  levelsOf,
  nodeAriaLabel,
  nodeData,
  orderLevels,
  parseMatrixBindings,
  sameNodeData,
  toDefinition,
  topologyKey,
  wouldCycle,
} from "@/lib/dag-layout"

function step(
  id: string,
  needs: string[] = [],
  extra: Partial<StepDefinition> = {}
): StepDefinition {
  return { id, name: id, needs, run: `echo ${id}`, ...extra }
}

const diamond = [
  step("checkout"),
  step("lint", ["checkout"]),
  step("test", ["checkout"]),
  step("package", ["lint", "test"]),
]

describe("levelsOf", () => {
  it("puts a step one column right of its furthest dependency", () => {
    const levels = levelsOf(diamond)
    expect(levels.get("checkout")).toBe(0)
    expect(levels.get("lint")).toBe(1)
    expect(levels.get("test")).toBe(1)
    expect(levels.get("package")).toBe(2)
  })

  it("uses the longest path, not the first one found", () => {
    // `late` depends on a root and on a step two columns in; it belongs after both.
    const levels = levelsOf([...diamond, step("late", ["checkout", "package"])])
    expect(levels.get("late")).toBe(3)
  })

  it("terminates on a cycle instead of recursing forever", () => {
    const levels = levelsOf([step("a", ["b"]), step("b", ["a"])])
    expect(levels.size).toBe(2)
  })

  it("ignores a dependency on a step that is not in the pipeline", () => {
    expect(levelsOf([step("only", ["ghost"])]).get("only")).toBe(0)
  })
})

describe("orderLevels", () => {
  it("reorders a column so edges stop crossing", () => {
    // Authored back-to-front: a→a2, b→b2 written so the two edges must cross.
    const steps = [step("a"), step("b"), step("b2", ["b"]), step("a2", ["a"])]
    const columns = orderLevels(steps, levelsOf(steps))
    expect(columns[0]).toEqual(["a", "b"])
    // The barycentre sweep pulls a2 above b2 to match its parent's row.
    expect(columns[1]).toEqual(["a2", "b2"])
  })
})

describe("layout", () => {
  it("places each column one COL_WIDTH further right", () => {
    const { nodes } = layout(diamond)
    const x = (id: string) => nodes.find((n) => n.id === id)!.position.x
    expect(x("lint") - x("checkout")).toBe(COL_WIDTH)
    expect(x("package") - x("lint")).toBe(COL_WIDTH)
  })

  it("centres a short column against the tallest one", () => {
    const { nodes } = layout(diamond)
    const y = (id: string) => nodes.find((n) => n.id === id)!.position.y
    const branchMid = (y("lint") + y("test")) / 2
    expect(y("checkout")).toBeCloseTo(branchMid)
    expect(y("package")).toBeCloseTo(branchMid)
  })

  it("builds one edge per dependency and drops dangling ones", () => {
    const { edges } = layout([...diamond, step("orphan", ["ghost"])])
    expect(edges).toHaveLength(4)
    expect(edges.some((e) => e.target === "orphan")).toBe(false)
  })

  it("marks the selected step and carries status through", () => {
    const { nodes } = layout(diamond, { lint: "failed" }, "lint")
    const lint = nodes.find((n) => n.id === "lint")!
    expect(lint.selected).toBe(true)
    expect(lint.data.status).toBe("failed")
  })
})

describe("nodeData", () => {
  it("advertises the fan-out of an authored matrix", () => {
    const data = nodeData(
      step("build", [], {
        matrix: { os: ["linux", "macos"], rust: ["stable"] },
      })
    )
    expect(data.matrixCells).toBe(2)
    expect(data.matrixAxes).toEqual(["os", "rust"])
    expect(data.matrixBinding).toBeUndefined()
  })

  it("shows the binding of an already-expanded cell instead", () => {
    const data = nodeData(
      step("build", [], { matrix: { os: ["linux"], rust: ["stable"] } })
    )
    expect(data.matrixBinding).toBe("os: linux, rust: stable")
    expect(data.matrixCells).toBeUndefined()
  })

  it("leaves matrix fields off a step without one", () => {
    const data = nodeData(step("plain"))
    expect(data.matrixCells).toBeUndefined()
    expect(data.matrixBinding).toBeUndefined()
  })
})

describe("topologyKey", () => {
  it("is stable when only data changes", () => {
    const before = topologyKey([step("a"), step("b", ["a"])])
    const after = topologyKey([
      step("a", [], { name: "renamed", run: "echo other" }),
      step("b", ["a"], { retries: 3 }),
    ])
    expect(after).toBe(before)
  })

  it("changes when a dependency is added", () => {
    expect(topologyKey([step("a"), step("b")])).not.toBe(
      topologyKey([step("a"), step("b", ["a"])])
    )
  })
})

describe("toDefinition", () => {
  const definition: PipelineDefinition = {
    name: "demo",
    env: { CI: "1" },
    timeout_minutes: 30,
    on: { cron: "0 0 * * * *" },
    workspace: { repo: "https://example.com/r.git", ref: "main" },
    steps: [
      step("a", [], {
        env: { LEVEL: "debug" },
        shell: "bash",
        working_directory: "apps/ui",
        continue_on_error: true,
        timeout_minutes: 15,
        secrets: [],
        artifacts: ["out/*.tgz"],
      }),
      step("b"),
    ],
  }

  const nodesOf = (ids: string[]) =>
    ids.map((id) => ({ id, position: { x: 0, y: 0 }, data: { label: id } }))

  it("keeps pipeline fields the canvas does not model", () => {
    const next = toDefinition(
      nodesOf(["a", "b"]),
      [{ id: "a-b", source: "a", target: "b" }],
      definition
    )
    expect(next.env).toEqual({ CI: "1" })
    expect(next.timeout_minutes).toBe(30)
    expect(next.on).toEqual(definition.on)
    expect(next.workspace).toEqual(definition.workspace)
    expect(next.name).toBe("demo")
  })

  it("keeps every step field the canvas does not model", () => {
    const next = toDefinition(
      nodesOf(["a", "b"]),
      [{ id: "a-b", source: "a", target: "b" }],
      definition
    )
    const a = next.steps.find((s) => s.id === "a")!
    expect(a.env).toEqual({ LEVEL: "debug" })
    expect(a.shell).toBe("bash")
    expect(a.working_directory).toBe("apps/ui")
    expect(a.continue_on_error).toBe(true)
    expect(a.timeout_minutes).toBe(15)
    expect(a.secrets).toEqual([])
    expect(a.artifacts).toEqual(["out/*.tgz"])
  })

  it("takes needs from the edges", () => {
    const next = toDefinition(
      nodesOf(["a", "b"]),
      [{ id: "a-b", source: "a", target: "b" }],
      definition
    )
    expect(next.steps.find((s) => s.id === "b")!.needs).toEqual(["a"])
    expect(next.steps.find((s) => s.id === "a")!.needs).toEqual([])
  })

  it("drops a deleted step and any need pointing at it", () => {
    const next = toDefinition(
      nodesOf(["b"]),
      [{ id: "a-b", source: "a", target: "b" }],
      definition
    )
    expect(next.steps.map((s) => s.id)).toEqual(["b"])
    expect(next.steps[0].needs).toEqual([])
  })

  it("gives a step added on the canvas a runnable default", () => {
    const next = toDefinition(nodesOf(["a", "b", "fresh"]), [], definition)
    const fresh = next.steps.find((s) => s.id === "fresh")!
    expect(fresh.run).toBeTruthy()
    expect(fresh.labels).toEqual(["os=linux"])
  })
})

describe("wouldCycle", () => {
  const chain = [
    { source: "checkout", target: "build" },
    { source: "build", target: "test" },
    { source: "test", target: "package" },
  ]

  it("refuses an edge that closes a loop, however long the path", () => {
    expect(wouldCycle(chain, "package", "checkout")).toBe(true)
    expect(wouldCycle(chain, "test", "build")).toBe(true)
  })

  it("refuses a self-edge", () => {
    expect(wouldCycle(chain, "build", "build")).toBe(true)
  })

  it("allows an edge that keeps the graph acyclic", () => {
    expect(wouldCycle(chain, "checkout", "package")).toBe(false)
    expect(wouldCycle(chain, "build", "package")).toBe(false)
  })

  it("allows an edge into a step nothing depends on yet", () => {
    expect(wouldCycle(chain, "package", "docs")).toBe(false)
  })

  it("terminates when the existing edges already contain a loop", () => {
    const looped = [
      { source: "a", target: "b" },
      { source: "b", target: "a" },
    ]
    expect(wouldCycle(looped, "b", "c")).toBe(false)
  })
})

describe("parseMatrixBindings", () => {
  it("reads the bindings the compiler renders into a cell's name", () => {
    expect(
      parseMatrixBindings("os: linux, rust: stable", ["os", "rust"])
    ).toEqual({
      os: ["linux"],
      rust: ["stable"],
    })
  })

  it("keeps a comma that belongs to the value", () => {
    // `fiber-core` joins pairs with ", ", so splitting on every comma would read this
    // cell as `args: -a` and silently drop the rest.
    expect(
      parseMatrixBindings("args: -a, -b, os: linux", ["args", "os"])
    ).toEqual({
      args: ["-a, -b"],
      os: ["linux"],
    })
  })

  it("handles a single axis", () => {
    expect(parseMatrixBindings("os: linux", ["os"])).toEqual({ os: ["linux"] })
  })

  it("uses the axes in sorted order, matching the compiler's BTreeMap", () => {
    expect(parseMatrixBindings("a: 1, b: 2", ["b", "a"])).toEqual({
      a: ["1"],
      b: ["2"],
    })
  })

  it("gives up rather than guessing when the text is not the expected shape", () => {
    expect(parseMatrixBindings("something else", ["os"])).toEqual({})
    expect(parseMatrixBindings("os: linux", ["os", "rust"])).toEqual({})
  })
})

describe("nodeAriaLabel", () => {
  it("names the step and its status, since the node itself announces neither", () => {
    expect(nodeAriaLabel(step("build"), "running")).toBe(
      "build, status running"
    )
  })

  it("says so when a step has not started", () => {
    expect(nodeAriaLabel(step("build"))).toBe("build, not started")
  })

  it("counts dependencies, singular and plural", () => {
    expect(nodeAriaLabel(step("sign", ["build"]))).toBe(
      "sign, not started, needs 1 step"
    )
    expect(nodeAriaLabel(step("ship", ["build", "sign"]), "queued")).toBe(
      "ship, status queued, needs 2 steps"
    )
  })

  it("falls back to the id when a step carries no name", () => {
    expect(nodeAriaLabel(step("step_a1b2", [], { name: "" }))).toBe(
      "step_a1b2, not started"
    )
  })
})

describe("sameNodeData", () => {
  const build = step("build", ["checkout"], {
    labels: ["os=linux"],
    artifacts: ["out/"],
  })

  it("holds for two builds of the same step and status", () => {
    expect(
      sameNodeData(nodeData(build, "running"), nodeData(build, "running"))
    ).toBe(true)
  })

  it("notices a status change, which is the whole point of polling", () => {
    expect(
      sameNodeData(nodeData(build, "running"), nodeData(build, "succeeded"))
    ).toBe(false)
  })

  it("compares list fields by content, not identity", () => {
    const again = step("build", ["checkout"], {
      labels: ["os=linux"],
      artifacts: ["out/"],
    })
    expect(sameNodeData(nodeData(build), nodeData(again))).toBe(true)
    const relabelled = step("build", ["checkout"], {
      labels: ["os=mac"],
      artifacts: ["out/"],
    })
    expect(sameNodeData(nodeData(build), nodeData(relabelled))).toBe(false)
  })

  it("notices an edit to the command or the condition", () => {
    expect(
      sameNodeData(nodeData(build), nodeData({ ...build, run: "make" }))
    ).toBe(false)
    expect(
      sameNodeData(nodeData(build), nodeData({ ...build, if: "always()" }))
    ).toBe(false)
  })
})
