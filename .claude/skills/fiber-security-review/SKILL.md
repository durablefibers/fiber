---
name: fiber-security-review
description: Security review checklist for Fiber, tuned to a self-hosted CI system that executes repo-supplied shell, injects project secrets as environment variables, mints agent and session tokens, and verifies GitHub webhooks. Use before merging changes to auth, roles, secrets, tokens, webhooks, artifacts, or agent execution.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout.
metadata:
  author: durablefibers
  version: "1.0"
---

# Fiber security review

Fiber's job is to run arbitrary code from a repository. **That is not the finding.** The review is about the boundary around it: whose code runs where, which secrets it can reach, and what a step can touch that it should not.

## Changes under review

```!
git diff --stat HEAD
```

## The current design (verify changes against this, do not re-derive it)

| Surface | Location | Design |
|---|---|---|
| Session auth | `fiber-api/src/auth.rs`, `store.rs` | Bearer `fiber_sess_*`, 32 random bytes, SHA-256 at rest; passwords argon2id with a legacy `salt$sha256` verify path |
| Agent auth | `auth.rs` (`AuthAgent`), `fiber-core/src/tokens.rs` | `fiber_agent_*`, 32 random bytes, SHA-256 at rest, also accepted as `/ws/agent?token=` |
| Roles | `access.rs`, `roles.rs` | `reader` < `writer` < `admin` < `owner` |
| Secrets at rest | `fiber-core/src/secrets.rs` | AES-256-GCM under `FIBER_SECRETS_KEY`, `enc:v1:` prefix; **plaintext when unset** (documented dev default) |
| Webhooks | `routes.rs::verify_github_sig` | HMAC-SHA256 over the raw body vs `x-hub-signature-256`, `constant_time_eq` comparison |
| Artifact paths | `artifact_util.rs` | `sanitize_artifact_rel_path` rejects `..` and absolute paths, filters to `[A-Za-z0-9._/-]`; caps 64MB HTTP / 8MB WS |

## Checklist

1. **Secret leakage into logs.** Project secrets are injected into steps as env vars, and the agent streams stdout/stderr verbatim into `log_lines`, readable by any project `reader`. There is no redaction layer. Flag anything that widens this: secrets in offers or events, secrets in error strings or tracing fields, log access below `reader`, or new secret-bearing env.
2. **Cross-project reach.** A step may run on a *global* agent. Offers must carry only the requesting project's secrets and artifacts; agent-authenticated endpoints must scope every read and write to the leased step's project.
3. **Auth confusion.** Agent tokens and session tokens are separate schemes. Neither may be accepted where the other is expected.
4. **Role gates.** Any handler resolving a project/pipeline/run/step/artifact/fiber id without `require_*`, any mutation gated at `reader`, and any case where the object lookup and the role check use different project ids.
5. **Token handling.** SHA-256 at rest, constant-time comparison, rotation invalidates the old value, never logged, never returned after creation, never added to a new URL (the existing `?token=` on the agent WS is accepted; do not widen the pattern).
6. **Webhook verification.** Signature checked against the **raw** body before any parse or side effect, failing closed when no secret is configured, constant-time. Check that `GITHUB_TOKEN` resolution cannot be steered by attacker-controlled payload fields.
7. **Artifact traversal and quota.** Every artifact write routes through `sanitize_artifact_rel_path` and honors the caps. S3 keys must not be built from unsanitized names. Presigned URLs need bounded expiry and must not be mintable for another project's objects.
8. **Secrets encryption.** Do not make the plaintext fallback harder to notice, drop the `enc:v1:` prefix, or break key rotation.
9. **SQL injection.** All queries are runtime `sqlx` with binds — flag any interpolated SQL.
10. **Container escape surface.** Steps supply `image` from pipeline YAML when `FIBER_AGENT_USE_DOCKER` is on. Flag added mounts, `--privileged`, docker-socket exposure, or host networking.

## Reporting

Order by severity. Each finding: `file:line`, vulnerability class, a concrete attacker story (who, what they control, what they gain), and the smallest correct fix.

Keep **accepted design risks** separate from **regressions introduced here**. Plaintext secrets without `FIBER_SECRETS_KEY` is documented and accepted; report it only if the change makes it worse or harder to detect. Do not pad the report with the fact that CI runs untrusted code by design.
