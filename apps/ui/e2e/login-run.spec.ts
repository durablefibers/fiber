import { type APIRequestContext, expect, request, test } from "@playwright/test"

// Log in through the UI, start a pipeline from its page, and watch the run page reach
// `succeeded` — the path a person actually takes, which no API-level smoke exercises.
const API = process.env.FIBER_E2E_API ?? "http://localhost:18080"
const USER = process.env.FIBER_E2E_USER ?? "admin"
const PASSWORD = process.env.FIBER_E2E_PASSWORD ?? "fiber"
const MARKER = "hello from the browser test"

let api: APIRequestContext
let projectId = ""
let pipelineId = ""

test.beforeAll(async () => {
  const login = await (await request.newContext({ baseURL: API })).post(
    "/api/auth/login",
    { data: { username: USER, password: PASSWORD } }
  )
  expect(login.ok()).toBeTruthy()
  const { token } = await login.json()
  api = await request.newContext({
    baseURL: API,
    extraHTTPHeaders: { Authorization: `Bearer ${token}` },
  })
  const project = await api.post("/api/projects", {
    data: { name: "Browser test", slug: `browser-${Date.now()}` },
  })
  expect(project.ok()).toBeTruthy()
  projectId = (await project.json()).id
  const pipeline = await api.post(`/api/projects/${projectId}/pipelines`, {
    data: {
      name: "browser",
      definition: {
        name: "browser",
        steps: [
          {
            id: "hello",
            name: "hello",
            needs: [],
            labels: ["os=linux"],
            run: `echo ${MARKER}`,
          },
        ],
      },
    },
  })
  expect(pipeline.ok()).toBeTruthy()
  pipelineId = (await pipeline.json()).id
})

test.afterAll(async () => {
  if (projectId) await api.delete(`/api/projects/${projectId}`)
})

test("log in, run a pipeline, watch it succeed", async ({ page }) => {
  await page.goto("/login")
  await page.locator('input[autocomplete="username"]').fill(USER)
  await page.locator('input[autocomplete="current-password"]').fill(PASSWORD)
  await page.getByRole("button", { name: "Sign in" }).click()
  await expect(page).not.toHaveURL(/\/login/)

  await page.goto(`/p/${projectId}/pipelines/${pipelineId}`)
  await page.getByRole("button", { name: "Run", exact: true }).click()
  await expect(page).toHaveURL(/\/runs\/[0-9a-f-]+/)
  await expect(
    page.getByText("succeeded", { exact: true }).first()
  ).toBeVisible({
    timeout: 90_000,
  })
  await expect(page.getByText(MARKER).first()).toBeVisible({ timeout: 15_000 })
})
