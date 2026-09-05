# Auth & project roles

## Authentication

- `POST /api/auth/login` `{ "username", "password" }` → session token  
- `Authorization: Bearer <token>` on API calls  
- `POST /api/auth/logout` invalidates the session  
- Passwords: **argon2id** (legacy SHA-256 hashes verify and upgrade on login)  
- Default bootstrap user: `FIBER_ADMIN_USER` / `FIBER_ADMIN_PASSWORD` (admin / fiber)

WebSocket `/ws/runs/{id}?token=<session>` requires a valid session and **reader** on the run’s project.

Agent auth uses a separate agent token (`/ws/agent?token=` or Bearer on agent HTTP routes).

## Roles

Per-project membership: `reader` < `writer` < `admin` < `owner`.

| Capability | reader | writer | admin | owner |
|---|---|---|---|---|
| View project, pipelines, runs, logs, artifacts, fibers, members | ✓ | ✓ | ✓ | ✓ |
| Create/update pipelines, start/cancel runs, fibers | | ✓ | ✓ | ✓ |
| Secrets, webhook secret, manage members | | | ✓ | ✓ |
| Grant/remove **owner**, create users for invites* | | | | ✓ |

\* `POST /api/users` requires the caller to be an **owner** on at least one project.

Creating a project makes the creator **owner**. Showcase seed grants the admin user owner.

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

Project secrets (`POST/GET/DELETE /api/projects/{id}/secrets`) inject as env vars into every step.

- At rest: AES-GCM when `FIBER_SECRETS_KEY` is 64 hex chars; otherwise plaintext (dev warning)
- Admins+ only to list/mutate
- Special: `GITHUB_TOKEN` / `FIBER_GITHUB_TOKEN` used for PR file listing — see [Triggers](./triggers.md)
