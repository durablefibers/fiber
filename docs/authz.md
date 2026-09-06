# Auth & project roles

## Authentication

- `POST /api/auth/login` `{ "username", "password" }` → session token  
- `Authorization: Bearer <token>` on API calls  
- `POST /api/auth/logout` invalidates the session  
- Passwords: **argon2id** (legacy SHA-256 hashes verify and upgrade on login)  
- Brute-force guard: 10 failed logins for a username within 10 minutes → that username is locked for 60 s (`429` + `Retry-After`; ~10 guesses/minute sustained). Keyed by username, so a known username can be held locked by an attacker — accepted over IP keying, which is spoofable behind a proxy  
- Default bootstrap user: `FIBER_ADMIN_USER` / `FIBER_ADMIN_PASSWORD` (admin / fiber) — also the first **instance admin**

WebSocket `/ws/runs/{id}?token=<session>` requires a valid session and **reader** on the run’s project.

Agent auth uses a separate agent token (`/ws/agent?token=` or Bearer on agent HTTP routes).

## Roles

Per-project membership: `reader` < `writer` < `admin` < `owner`.

| Capability | reader | writer | admin | owner |
|---|---|---|---|---|
| View project, pipelines, runs, logs, artifacts, fibers, members | ✓ | ✓ | ✓ | ✓ |
| Create/update pipelines, start/cancel runs, fibers | | ✓ | ✓ | ✓ |
| Secrets, webhook secret, manage members | | | ✓ | ✓ |
| Grant/remove **owner** | | | | ✓ |

Creating a project makes the creator **owner**. Showcase seed grants the admin user owner.

## Instance admin

Separate from project roles: `users.is_admin`. Project routes (pipelines, runs, secrets, members) still require membership — an instance admin who is not a member of a project gets 403 on those. But instance admins manage **every agent**, and an agent token receives a project's secrets in its offers, so in practice the flag is **root**: grant it only to operators.

| Capability | instance admin |
|---|---|
| Register / update / delete / rotate **global** agents (`project_id` null) | ✓ |
| `GET /api/agents` without a `project_id` filter (every agent in the instance) | ✓ |
| Manage any **project** agent (project admins can manage their own) | ✓ |
| `GET /api/users`, `POST /api/users`, `PUT /api/users/{id}` `{ is_admin }` | ✓ |

Why: a global agent's token leases steps — and receives the injected secrets — from **every** project, so minting one must not be available to an ordinary member.

- Fresh installs: the bootstrap user is the first instance admin. Existing installs: nobody is promoted by the migration; at boot, `fiber-api` promotes the user named by `FIBER_ADMIN_USER` **only while the instance has no admin at all** (a recovery path — it is never re-applied on every boot, so a demotion sticks and a squatted username gains nothing). If no admin exists and `FIBER_ADMIN_USER` matches no user, the API logs an error telling you to point it at an existing username.
- The last instance admin cannot be demoted (`PUT /api/users/{id}` → 400; enforced inside the UPDATE, so concurrent demotions cannot race to zero).
- Project admins still invite users via `POST /api/projects/{id}/members` with a `password` — that path is project-scoped and unchanged.
- `PublicUser` (login response, `/api/auth/me`, `GET /api/users`) carries `is_admin` so the UI can hide the global-agent form. Project member lists do not expose it.

## Members API

```
GET    /api/projects/{id}/members
POST   /api/projects/{id}/members     { username, role, password? }
PUT    /api/projects/{id}/members/{user_id}  { role }
DELETE /api/projects/{id}/members/{user_id}
```

`POST` can create a user if `password` is provided and the username does not exist. Cannot remove the last owner.

CLI: `fiber members …` — [cli.md](./cli.md).

## Secrets

Project secrets (`POST/GET/DELETE /api/projects/{id}/secrets`) inject as env vars. By default every step of the project receives all of them; a step can narrow that with `secrets:` in the pipeline (see [pipeline-yaml](./pipeline-yaml.md)). Agents mask secret values in log lines and pass them to containers through a private env-file rather than the command line.

- At rest: AES-GCM when `FIBER_SECRETS_KEY` is 64 hex chars; otherwise plaintext (dev warning)
- Admins+ only to list/mutate
- Special: `GITHUB_TOKEN` / `FIBER_GITHUB_TOKEN` used for PR file listing — see [Triggers](./triggers.md)
