# fiber CLI

Binary: `fiber` (`cargo run -p fiber-cli -- …`). Auth: `fiber login` writes `~/.fiber/token`, or set `FIBER_TOKEN` / `--token`. API base: `FIBER_API_URL` (default `http://127.0.0.1:18080`).

## Commands

| Command | Purpose |
|---|---|
| `validate [fiber.yml]` | Parse + compile DAG locally |
| `login` | Session token → stdout + `~/.fiber/token` |
| `run <pipeline_id>` | Start a manual run |
| `members list\|add\|set\|remove` | Project membership |
| `secrets list\|set\|delete` | Project secrets (admin) |
| `agents list\|create\|update\|delete\|rotate` | Agent registrations |
| `agent` | Spawn `fiber-agent` with token/labels |
| `fibers list\|create\|get\|cancel` | Durable control-plane tasks |

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
fiber secrets delete $PROJECT_ID GITHUB_TOKEN
```

## Agents

```bash
fiber agents list
fiber agents list --project-id $PROJECT_ID          # project + globals
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
