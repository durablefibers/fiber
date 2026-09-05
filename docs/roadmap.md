# Roadmap

Shipped: MVP DAG CI, agents (global + project pools), artifacts (local + S3 presign), authz, retention, GitHub push/PR + path filters, cron/interval, matrix/`if`, durable fibers, CLI, docs, dogfoods.

## Later

| Area | Ideas |
|---|---|
| **SCM** | GitLab / Bitbucket webhooks; multibranch indexing |
| **Secrets** | Vault / OIDC / external secret stores (beyond encrypted project secrets) |
| **Agents** | Dynamic cloud agents; autoscaling pools |
| **Extensibility** | Plugin / WASM step SDK |
| **Product** | Preview envs, BYOC, managed DBs, GPU (Northflank-class — out of scope for self-hosted CI core) |

These are intentional non-goals for the current self-hosted CI core. Revisit when the vertical slice is in production use.
