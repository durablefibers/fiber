# fiber CLI

Binary: `fiber` (`cargo run -p fiber-cli -- …`). Auth: `fiber login` writes `~/.fiber/token`, or set `FIBER_TOKEN` / `--token`. API base: `FIBER_API_URL` (default `http://127.0.0.1:18080`).

## Commands

Add `--json` to any command for machine-readable output.

| Command | Purpose |
|---|---|
| `validate [fiber.yml]` | Parse + compile DAG locally |
| `login` / `logout` | Session token → stdout + `~/.fiber/token` (dir `0700`, file `0600`). `--password-stdin` keeps the password out of shell history; `logout` deletes the file |
| `projects list\|create` | Projects you can see |
| `pipelines list\|get\|apply` | `apply` pushes a `fiber.yml` — create or update |
| `run <pipeline_id>` | Start a manual run; `--wait` / `--follow` block and exit with the outcome |
| `runs list\|get\|cancel\|retry` | Run history and control |
| `logs <step_run_id>` | Step output; `--attempt N`, `--follow` |
| `artifacts list\|download` | Run artifacts |
| `completions <shell>` | Completion script for bash, zsh, fish, elvish, powershell |
| `members list\|add\|set\|remove` | Project membership |
| `secrets list\|set\|delete` | Project secrets (admin) |
| `agents list\|create\|update\|delete\|rotate` | Agent registrations |
| `agent` | Spawn `fiber-agent` with token/labels |
| `fibers list\|create\|get\|cancel` | Durable control-plane tasks |

## CI as code

```bash
fiber pipelines apply fiber.yml --project-id $PROJECT_ID   # create, or update by name
fiber pipelines apply fiber.yml --id $PIPELINE_ID          # update a specific one
```

The file is parsed and the DAG compiled locally first, so a syntax or cycle error fails
before anything is sent.

## Running from another CI system, or a git hook

```bash
fiber run $PIPELINE_ID --follow           # stream output, block until finished
echo $?                                   # 0 succeeded · 1 failed/cancelled · 3 timed out
```

`--wait` is the same without the output. Step transitions go to stderr and log lines to
stdout, so `fiber run … --follow > build.log` keeps the two apart. Waiting polls the API
rather than holding a WebSocket, so it works behind proxies that do not pass upgrades.

```bash
fiber runs list $PROJECT_ID --limit 20                 # newest first
fiber runs list $PROJECT_ID --before $LAST_RUN_ID      # next page
fiber runs get $RUN_ID
fiber runs retry $RUN_ID --failed-only --wait          # re-run only what failed
fiber runs cancel $RUN_ID
```

## Logs and artifacts

```bash
fiber logs $STEP_RUN_ID                # newest 1000 lines
fiber logs $STEP_RUN_ID --attempt 2    # one attempt (seq restarts per attempt)
fiber logs $STEP_RUN_ID --follow       # tail until the step finishes

fiber artifacts list $RUN_ID
fiber artifacts download $ARTIFACT_ID --out ./dist/app.tar
```

## Shell completion

```bash
fiber completions zsh  > ~/.zfunc/_fiber
fiber completions bash > /etc/bash_completion.d/fiber
```

## Members

```bash
fiber members list $PROJECT_ID
fiber members add $PROJECT_ID --username alice --role writer --password secret
fiber members set $PROJECT_ID $USER_ID --role admin
fiber members remove $PROJECT_ID $USER_ID
```

Roles: `reader` < `writer` < `admin` < `owner`.

## Secrets

```bash
fiber secrets list $PROJECT_ID
fiber secrets set $PROJECT_ID GITHUB_TOKEN --value ghp_…
printf '%s' "$GH_TOKEN" | fiber secrets set $PROJECT_ID GITHUB_TOKEN --value-stdin   # no argv / history exposure
fiber secrets delete $PROJECT_ID GITHUB_TOKEN
```

## Agents

```bash
fiber agents list                                   # every agent — instance admin only
fiber agents list --project-id $PROJECT_ID          # project + globals (project reader)
fiber agents create --name local --labels os=linux,docker=true
fiber agents create --name team --labels os=linux --project-id $PROJECT_ID
fiber agents update $AGENT_ID --concurrency 2
fiber agents rotate $AGENT_ID                       # new token once on stdout
fiber agents delete $AGENT_ID

# Run the worker binary
export FIBER_AGENT_TOKEN=…   # from create/rotate
fiber agent --labels os=linux
```

`create` / `rotate` print the plaintext token on **stdout** once; agent JSON goes to stderr.

## Fibers

See [durable-fibers.md](./durable-fibers.md).
