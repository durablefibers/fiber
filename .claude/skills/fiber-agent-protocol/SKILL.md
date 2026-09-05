---
name: fiber-agent-protocol
description: Changing the agent WebSocket protocol in fiber-proto and keeping all four consumers in step — fiber-api, fiber-agent, fiber-cli, and the hand-mirrored TypeScript client. Use when adding a message variant or field, changing offers, leases, log streaming, or artifact upload, or when an agent and API disagree at runtime.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout and Rust stable.
metadata:
  author: durablefibers
  version: "1.0"
---

# Changing the agent protocol

`fiber-proto` is small and load-bearing. Every type in it is serialized across a process boundary, so a change is never local.

## The four consumers

| Consumer | What breaks if you skip it |
|---|---|
| `crates/fiber-api/src/ws.rs` | Server side of `/ws/agent`: builds offers, handles agent messages |
| `crates/fiber-agent/src/main.rs` | Client side: claims offers, streams logs, uploads artifacts |
| `crates/fiber-cli/src/main.rs` | Spawns an agent and validates YAML against the same definition types |
| `apps/web/src/lib/api.ts` | Hand-mirrors these shapes for the UI — **no codegen, silent drift** |

`fiber-scheduler` also constructs `ServerMessage` values when dispatching offers.

## The compatibility rule that actually matters

**An old agent talks to a new API and vice versa.** Agents are separate long-lived processes on other machines, often not upgraded in lockstep with the control plane. Therefore:

- Add new fields as `Option<T>` or with `#[serde(default)]`. A required new field on an existing message breaks every deployed agent instantly.
- Add new message variants rather than repurposing existing ones, and make unknown variants non-fatal on the receiving side where possible.
- Never rename a serialized field without a transition release — the wire name is the contract, not the Rust identifier.
- Never change the meaning of an existing field while keeping its name and type. That is the failure mode no test catches.

## Procedure

1. Change the type in `crates/fiber-proto/src/lib.rs` (`AgentMessage`, `ServerMessage`, `RunEvent`, `WorkspaceOffer`, `ArtifactRestore`, `StepDefinition`, `PipelineDefinition`).
2. Update the API side: `ws.rs` for the protocol, `fiber-scheduler` where offers are built.
3. Update `fiber-agent`.
4. Update `fiber-cli` if it touches the type.
5. Update `apps/web/src/lib/api.ts` if the shape reaches the UI (`RunEvent` does, via `/ws/runs/{id}`).
6. `docs/agents.md` for protocol behavior, `docs/pipeline-yaml.md` for definition fields.

## Verify end-to-end, not by compiling

A green build proves nothing about the wire. Run a real step through:

```bash
make infra && make api                                    # terminal 1
cargo run -p fiber-cli -- agents create ...               # get a token
FIBER_AGENT_TOKEN=... make agent                          # terminal 2
make dogfood-artifacts                                    # exercises offer -> exec -> logs -> artifact
```

`DOGFOOD_OK` is the pass condition. To check backward compatibility explicitly, run an agent built from the previous commit against the new API.

## Two constraints to remember

- Log chunks and WS artifact uploads have size limits (`MAX_WS_ARTIFACT_BYTES`, 8MB, versus 64MB for the HTTP path). Adding payload to a per-chunk message multiplies across every log line of every step.
- Artifacts are uploaded **on success only**, and paths go through `sanitize_artifact_rel_path`.
