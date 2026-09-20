# Web UI

`apps/ui` — TanStack Start + React Flow, served on `:3100` (`make ui` in development,
the `fiber-ui` Compose service in a deployment). It talks to `fiber-api` at
`VITE_FIBER_API_URL`, baked in at build time. Login defaults are **admin / fiber**.

## Pages

| Path | What it is for |
|---|---|
| `/` | Every project you can see; create one |
| `/agents` | The global agent pool — create, relabel, rotate tokens, delete |
| `/settings` | Your account: password, sign out other sessions. Instance admins also manage users |
| `/p/{project}` | Project overview: pipelines with their last run, and recent runs |
| `/p/{project}/runs` | Every run, newest first, filterable, paged with **Load more** |
| `/p/{project}/runs/{run}` | One run: canvas, step detail, attempts, logs, artifacts |
| `/p/{project}/pipelines/{pipeline}` | The pipeline editor — canvas plus inspector |
| `/p/{project}/agents` | Agents dedicated to this project |
| `/p/{project}/fibers` | Durable fibers for this project |
| `/p/{project}/settings` | Members, project secrets, the GitHub webhook, and deleting the project |

Project-scoped pages appear in the sidebar under the project's name. Instance-wide
concerns (the global agent pool, your account, users) stay in the top group — a
project's secrets and members are never in the global Settings page.

Deleting a project is owner-only and irreversible: it takes every pipeline, run, log
line, artifact, secret, member, durable fiber, and project-scoped agent with it, and
cancels anything still running first. The button unlocks only once you type the
project's name. The seeded **showcase** project comes back on the next `fiber-api`
boot, so deleting it is a reset rather than a removal.

## The canvas

The same `DagCanvas` renders both the editor and a run, and the layout is layered:
a step sits one column right of its furthest dependency, and columns are ordered by
the barycentre of their neighbours so a fan-out reads as a fan rather than a tangle.

- **Tidy** re-runs the layout and re-fits the view. Nodes you drag stay where you put
  them until then — status updates during a live run never move them.
- **Full screen** — the button, or `F` once the keyboard is in the canvas — gives the
  whole viewport to the work area; `Escape` or **Exit** comes back. It is the canvas
  *and* its companion panel that expand, not the canvas alone, so selecting a step still
  reaches the inspector or the log stream. On the run page the artifact list steps aside
  to give the graph its room.
- The view re-fits whenever the canvas changes size, including an ordinary window
  resize — unless you have panned or zoomed it yourself, in which case your view is
  left alone until you ask for a new one.
- In the editor: drag between handles to add a `needs` edge, `Backspace`/`Delete` to
  remove a node or edge, **Add step** for a new one. A connection that would close a
  cycle is refused before it is drawn.
- A node shows its status dot, the first line of `run`, its labels, and badges for
  retries, a Docker `image`, declared `artifacts`, and `continue_on_error`.

### Matrix steps

A run's canvas is built from the run's **step runs** — the compiled DAG — not from the
definition snapshot. A step with a `matrix` therefore appears as its real cells, each
with its own status, logs, and attempts, tagged with its bindings (`os: linux`). In the
editor the same step is still one node, badged with how many cells it will expand to.

## The pipeline editor

The inspector has two tabs. **Pipeline** covers the workspace repo and ref, every
trigger (`push`, `pull_request`, `interval_minutes`, `cron`, and their path filters),
pipeline-wide `env`, and the run `timeout_minutes`. **Step** covers the step list, the
selected step's name, `run`, and `needs`, with the rest behind **Show advanced**:
`if`, `matrix`, `image`, `labels`, `retries`, `timeout_minutes`, `artifacts`, `shell`,
`working_directory`, `env`, `secrets`, and `continue_on_error`.

`secrets` is three-state, matching the YAML: **All** omits the field (every project
secret), **None** writes `[]`, **Pick** narrows to the names you list. See
[pipeline-yaml](./pipeline-yaml.md) for what each field means.

**Export** renders the definition as `fiber.yml`; **Import** parses pasted YAML through
`POST /api/pipelines/parse-yaml`, so the UI and the file format stay the same thing.
`⌘S` / `Ctrl+S` saves; leaving with unsaved changes prompts first.

## The run page

The canvas and artifacts sit on the left, the step inspector and logs on the right;
the two stack on a narrow screen. Selecting a step — on the canvas or in the step
strip — puts it in the URL (`?step=…`), so a link to one step's logs is shareable.
In **Full screen** the pair fills the viewport and the log pane stops scaling with the
window, so a wide DAG gets every pixel that is not the logs.

Logs stream over `/ws/runs/{id}` and follow the tail until you scroll up. The toolbar
filters lines, toggles wrapping, and copies what is shown. Because `seq` restarts each
attempt, logs are fetched per attempt: pick an attempt to read that one in isolation.
Only the newest attempt tails live; an older one is a fixed page.

The view is built for a build that talks faster than a browser can paint. Incoming lines
are committed once per animation frame rather than once per event, and only the rows on
screen are in the DOM, so a step printing thousands of lines a second does not stall the
tab. When the server says the stream lagged, or the socket drops and comes back, the page
refetches from the last line id it holds and re-reads the run's own state rather than
waiting for the next event — so a gap shows up as a pause, not as missing output or a
run stuck on "running".

**Re-run** starts a fresh run from the same snapshot; on a failed run, **Re-run failed
steps** carries the succeeded ones over.
