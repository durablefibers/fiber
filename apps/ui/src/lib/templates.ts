import type { PipelineDefinition } from "@/lib/api"

export type PipelineTemplate = {
  id: string
  name: string
  blurb: string
  definition: PipelineDefinition
}

export const PIPELINE_TEMPLATES: PipelineTemplate[] = [
  {
    id: "simple",
    name: "Simple linear",
    blurb: "checkout → build → test",
    definition: {
      name: "build-and-test",
      steps: [
        {
          id: "checkout",
          name: "checkout",
          needs: [],
          run: "ls -la && echo ready",
          labels: ["os=linux"],
        },
        {
          id: "build",
          name: "build",
          needs: ["checkout"],
          run: "echo building && sleep 0.2",
          labels: ["os=linux"],
        },
        {
          id: "test",
          name: "test",
          needs: ["build"],
          run: "echo testing && sleep 0.2",
          labels: ["os=linux"],
        },
      ],
    },
  },
  {
    id: "diamond",
    name: "Diamond CI",
    blurb: "Parallel lint / unit / types, then package + smoke",
    definition: {
      name: "diamond-ci",
      workspace: {
        repo: "https://github.com/octocat/Hello-World.git",
        ref: "master",
      },
      on: { push: { branches: ["main", "develop"] } },
      steps: [
        {
          id: "checkout",
          name: "checkout",
          needs: [],
          run: "ls -la && test -f README",
          labels: ["os=linux"],
          retries: 1,
        },
        {
          id: "lint",
          name: "lint",
          needs: ["checkout"],
          run: "echo linting && sleep 0.3",
          labels: ["os=linux"],
        },
        {
          id: "unit",
          name: "unit tests",
          needs: ["checkout"],
          run: "echo unit && sleep 0.4",
          labels: ["os=linux"],
          retries: 1,
        },
        {
          id: "types",
          name: "typecheck",
          needs: ["checkout"],
          run: "echo typecheck && sleep 0.25",
          labels: ["os=linux"],
        },
        {
          id: "package",
          name: "package",
          needs: ["lint", "unit", "types"],
          run: "mkdir -p dist && echo bundle > dist/app.txt",
          labels: ["os=linux"],
          artifacts: ["dist/app.txt"],
        },
        {
          id: "smoke",
          name: "smoke",
          needs: ["package"],
          run: "test -f dist/app.txt && echo smoke-ok",
          labels: ["os=linux"],
        },
      ],
    },
  },
  {
    id: "fanout",
    name: "Fan-out tests",
    blurb: "Matrix-style parallel lanes + report artifact",
    definition: {
      name: "fan-out-tests",
      on: { push: { branches: ["main"] } },
      steps: [
        {
          id: "checkout",
          name: "checkout",
          needs: [],
          run: "echo ready",
          labels: ["os=linux"],
        },
        {
          id: "test-linux",
          name: "test linux",
          needs: ["checkout"],
          run: "uname -s && sleep 0.3",
          labels: ["os=linux"],
          retries: 1,
        },
        {
          id: "test-docker",
          name: "test docker labels",
          needs: ["checkout"],
          run: "echo docker-lane && sleep 0.35",
          labels: ["os=linux", "docker=true"],
        },
        {
          id: "integration",
          name: "integration",
          needs: ["checkout"],
          run: "echo integration && sleep 0.5",
          labels: ["os=linux"],
        },
        {
          id: "report",
          name: "report",
          needs: ["test-linux", "test-docker", "integration"],
          run: "echo green > report.txt",
          labels: ["os=linux"],
          artifacts: ["report.txt"],
        },
      ],
    },
  },
  {
    id: "release",
    name: "Release train",
    blurb: "Build → sign → staging → canary → prod with artifacts",
    definition: {
      name: "release-with-artifacts",
      workspace: {
        repo: "https://github.com/octocat/Hello-World.git",
        ref: "master",
      },
      steps: [
        {
          id: "checkout",
          name: "checkout",
          needs: [],
          run: "test -f README",
          labels: ["os=linux"],
          retries: 1,
        },
        {
          id: "build",
          name: "build",
          needs: ["checkout"],
          run: "mkdir -p out && echo v1.0.0 > out/VERSION && tar -cf out/release.tar README out/VERSION",
          labels: ["os=linux"],
          artifacts: ["out/VERSION", "out/release.tar"],
        },
        {
          id: "sign",
          name: "sign",
          needs: ["build"],
          run: "echo signed > out/release.tar.sig",
          labels: ["os=linux"],
          artifacts: ["out/release.tar.sig"],
        },
        {
          id: "staging",
          name: "deploy staging",
          needs: ["sign"],
          run: "echo staging $(cat out/VERSION)",
          labels: ["os=linux"],
          retries: 1,
        },
        {
          id: "canary",
          name: "canary",
          needs: ["staging"],
          run: "sleep 0.4 && echo healthy",
          labels: ["os=linux"],
        },
        {
          id: "prod",
          name: "deploy prod",
          needs: ["canary"],
          run: "echo prod $(cat out/VERSION)",
          labels: ["os=linux"],
        },
      ],
    },
  },
  {
    id: "retry",
    name: "Retry & cascade",
    blurb: "Flaky step with retries; dependents skip on failure",
    definition: {
      name: "retry-and-skip",
      steps: [
        {
          id: "prep",
          name: "prep",
          needs: [],
          run: "mkdir -p .fiber && echo 0 > .fiber/attempt_hint",
          labels: ["os=linux"],
        },
        {
          id: "flaky",
          name: "flaky (retries)",
          needs: ["prep"],
          run: 'n=$(cat .fiber/attempt_hint); n=$((n+1)); echo $n > .fiber/attempt_hint; echo attempt=$n; [ "$n" -ge 2 ] || exit 1; echo recovered',
          labels: ["os=linux"],
          retries: 2,
          artifacts: [".fiber/attempt_hint"],
        },
        {
          id: "downstream",
          name: "downstream",
          needs: ["flaky"],
          run: "echo only-if-flaky-ok",
          labels: ["os=linux"],
        },
        {
          id: "notify",
          name: "notify",
          needs: ["downstream"],
          run: "echo notify",
          labels: ["os=linux"],
        },
      ],
    },
  },
]

export function emptyDefinition(name = "pipeline"): PipelineDefinition {
  const t = PIPELINE_TEMPLATES[0]!
  return { ...structuredClone(t.definition), name }
}

export function templateById(id: string): PipelineTemplate | undefined {
  return PIPELINE_TEMPLATES.find((t) => t.id === id)
}
