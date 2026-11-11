# Security

## Reporting a vulnerability

Report privately through GitHub: **Security → Advisories → Report a vulnerability** on
this repository. Please do not open a public issue.

Include what you did, what happened, and what you expected. A minimal reproduction against
a local `make infra && make api` stack is the most useful thing you can send. We aim to
acknowledge within a few days.

## Trust model

Fiber executes shell supplied by a repository. Several behaviours that look like findings
are the documented shape of the system, so it is worth being precise about where the
boundaries are.

**Inside the model — please report these.**

- Anything that crosses a **project** boundary: reading another project's secrets,
  artifacts, logs, or runs; leasing another project's steps.
- Anything that lets a role do what a higher role can: a `reader` writing, a `writer`
  reaching instance-level surfaces, a non-admin managing global agents or users.
- Authentication or session flaws: forging a session or agent token, bypassing webhook
  signature verification, session fixation.
- A **fork's** pull request obtaining project secrets, its run being scheduled onto an
  agent outside that project's own pool, or a step of it running on an agent's host shell
  instead of in a container.
- A Docker agent running a step on its host shell without
  `FIBER_AGENT_ALLOW_HOST_SHELL=true`.
- Secrets escaping where they should not go: into logs, into a process list, into an
  artifact, into an API response.
- Anything unauthenticated that starts a run, mutates state, or discloses data.

**Outside the model — these are by design.**

- **A project `writer` can run arbitrary code on any agent serving that project.** That is
  what a pipeline is. Give agents the trust you would give the people who can write a
  `fiber.yml`, and see [docs/agents.md](docs/agents.md) for how a step is confined.
- **An instance admin is effectively root.** They manage the global agent pool, and an
  agent token receives the secrets of the projects it serves.
- **Fork pull requests still execute untrusted code.** Fiber withholds secrets, restricts
  them to project-dedicated agents, and runs their steps in containers or not at all,
  which limits the blast radius; a container is not a sandbox. Run them on a disposable
  agent — [docs/triggers.md](docs/triggers.md#pull-requests-from-forks).
- **A host-shell step runs as the agent's user.** That is every step on a shell agent
  (`FIBER_AGENT_USE_DOCKER=false`), and a step without `image:` on a Docker agent whose
  operator set `FIBER_AGENT_ALLOW_HOST_SHELL=true` — where the agent's user can reach the
  Docker engine, so such a step is root on the host. A Docker agent refuses those steps
  by default.
- **Defaults are for a local trial, not the internet.** `admin` / `fiber`, no secrets
  encryption key, everything on loopback. [docs/operations.md](docs/operations.md#deployment)
  describes what to change before exposing an instance.

## Hardening a deployment

At minimum: set `FIBER_ADMIN_PASSWORD` before the first boot, generate `FIBER_SECRETS_KEY`
(`openssl rand -hex 32`) and back it up, terminate TLS in front of the API, set
`FIBER_CORS_ORIGINS`, and keep Postgres, Redis, and object storage off public interfaces.
The reference `deploy/docker-compose.yml` does the last part by default.

## Supported versions

Fiber is pre-1.0. Fixes land on `main` and go out in the next release; there are no
long-lived support branches yet.
