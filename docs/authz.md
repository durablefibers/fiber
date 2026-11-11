# Auth & project roles

## Authentication

- `POST /api/auth/login` `{ "username", "password" }` → session token  
- `Authorization: Bearer <token>` on API calls  
- `POST /api/auth/logout` invalidates the session  
- Passwords: **argon2id** (legacy SHA-256 hashes verify and upgrade on login)  
- Brute-force guard: 10 failed logins for a username within 10 minutes → that username is locked for 60 s (`429` + `Retry-After`). An attempt is reserved **before** the password is verified, and attempts still being verified count against the same 10, so parallel requests cannot outrun the lockout: at most 10 guesses per username before it locks, then at most 10 per 60 s. Keyed by username, so a known username can be held locked by an attacker — accepted over IP keying, which is spoofable behind a proxy
- Password hashing is bounded process-wide: at most **4** argon2id hashes or verifies run at once, across login, password change, and user creation. They run off the async workers, so a login flood cannot stall agent heartbeats, and a slot is held until the hash finishes even if the request that started it has gone. A request that finds all four busy waits its turn (first come, first served) for up to 5 s, then gets `503` with `Retry-After` and "server busy; nothing was checked" — never the `429` of a lockout, and never counted as a failed login. So a flood of made-up usernames slows logins down rather than refusing them: a real login waits roughly *(requests queued ahead ÷ 4) × one verify* (tens of milliseconds each). A flood that sustains more verifies than four slots can clear will push that past 5 s; rate-limit `/api/auth/login` per client at the reverse proxy to close that, since the API does not trust a forwarded client address
- Every wrong login costs one argon2id verify — unknown username, wrong password, or an account still on a legacy SHA-256 hash — so response time does not reveal which usernames exist
- The guard is in memory and per process: a restart clears it, and each `fiber-api` replica counts separately. It tracks up to 10,000 usernames; past that it drops expired entries, then those with the fewest failures — a flood of made-up usernames evicts itself before a real account's count
- Default bootstrap user: `FIBER_ADMIN_USER` / `FIBER_ADMIN_PASSWORD` (admin / fiber) — also the first **instance admin**. `fiber-api` listens on loopback by default and refuses to start on any other address while that default password still works, unless `FIBER_ALLOW_DEFAULT_ADMIN=1`

WebSocket `/ws/runs/{id}` takes the session token in the `Sec-WebSocket-Protocol` header as `fiber.token.<session>` (never the query string) and requires **reader** on the run’s project.

Agent auth uses a separate agent token (`/ws/agent?token=` or Bearer on agent HTTP routes).
The token decides **which** agent a connection is; the agent's database row decides what
that agent may be offered. Pool scope comes from the row alone. Labels and concurrency are
capped by the row: a connecting agent is registered with the row's labels that it also
claims and the smaller of the two concurrencies, so holding a token does not let its
holder claim labels or slots the agent was not registered with (a row with no labels at
all is the exception — see [agents](./agents.md#labels-and-concurrency-the-registration-is-the-ceiling)).

## Roles

Per-project membership: `reader` < `writer` < `admin` < `owner`.

| Capability | reader | writer | admin | owner |
|---|---|---|---|---|
| View project, pipelines, runs, logs, artifacts, fibers, members | ✓ | ✓ | ✓ | ✓ |
| Create/update pipelines, start/cancel runs, fibers | | ✓ | ✓ | ✓ |
| Secrets, webhook secret, manage members | | | ✓ | ✓ |
| Grant/remove **owner** | | | | ✓ |

Creating a project makes the creator **owner**. The showcase seed grants the bootstrap
admin owner of the demo project — which is why `showcase` is a **reserved slug** no
`POST /api/projects` may claim (400), and why the seed runs only on a fresh instance.
A caller-supplied `slug` is `slugify`d like a derived one, so it cannot contain anything
a generated slug could not.

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
- **Owner is the only role that can delete a project** (`DELETE /api/projects/{id}`). Instance admins get no exemption: like every project-scoped route, it requires membership. Deleting the showcase project is permanent: the seed runs only when the instance has no users, unless `FIBER_SEED_SHOWCASE=1` is set for a boot.
- `PublicUser` (login response, `/api/auth/me`, `GET /api/users`) carries `is_admin` so the UI can hide the global-agent form. Project member lists do not expose it.

## Members API

```
GET    /api/projects/{id}/members
POST   /api/projects/{id}/members     { username, role, password? }
PUT    /api/projects/{id}/members/{user_id}  { role }
DELETE /api/projects/{id}/members/{user_id}
```

`POST` can create a user if `password` is provided and the username does not exist.

Owners are protected from everyone below them: granting `owner`, changing an existing owner's role, or removing an owner all require the **actor** to be an owner (`403` otherwise), and the last owner can be neither demoted nor removed (`400`). The last-owner check runs inside the `UPDATE` / `DELETE` under a lock on the project's owner rows, so two concurrent removals cannot each see two owners and leave none.

CLI: `fiber members …` — [cli.md](./cli.md).

## Secrets

Project secrets (`POST/GET/DELETE /api/projects/{id}/secrets`) inject as env vars, except into runs built from a fork's pull request, which get none — see [triggers](./triggers.md). By default every step of the project receives all of them; a step can narrow that with `secrets:` in the pipeline (see [pipeline-yaml](./pipeline-yaml.md)). Agents mask secret values in log lines and pass them to containers through a private env-file rather than the command line.

- At rest: AES-GCM when `FIBER_SECRETS_KEY` is 64 hex chars; plaintext when it is unset (dev warning). A key that is set but invalid stops the API at boot — it is never treated as unset
- Admins+ only to list/mutate
- Special: `GITHUB_TOKEN` / `FIBER_GITHUB_TOKEN` used for PR file listing — see [Triggers](./triggers.md)

## Accounts

Changing a password is something you do to your **own** account: `POST /api/auth/password`,
with the current password. An instance admin manages the admin flag through
`PUT /api/users/{id}` and cannot set anyone's password — being able to would mean being able
to take over an account silently, which is a different power from managing one.

A password change drops every **other** session for that user, because a password change is
what someone does when they think a credential is compromised. The session that made the
change survives, so it does not log you out of the page you did it from.

`DELETE /api/auth/sessions` does the same without changing the password, for when a token
leaks rather than a password. Before this existed the only remedy was waiting out the
14-day expiry.
