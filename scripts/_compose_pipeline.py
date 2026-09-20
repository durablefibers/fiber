#!/usr/bin/env python3
"""Helpers for smoke_compose.sh: mint an agent, run one pipeline, clean up."""
from __future__ import annotations

import json
import sys
import time
import urllib.error
import urllib.request

API = "http://127.0.0.1:18080"
AGENT_NAME = "compose-smoke"
PIPELINE_NAME = "compose-smoke"


def req(method: str, path: str, token: str | None = None, body: dict | None = None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    r = urllib.request.Request(API + path, data=data, headers=headers, method=method)
    with urllib.request.urlopen(r, timeout=30) as resp:
        raw = resp.read().decode()
        return json.loads(raw) if raw else {}


def login() -> str:
    return req("POST", "/api/auth/login", body={"username": "admin", "password": "fiber"})["token"]


def showcase(token: str) -> str:
    projects = req("GET", "/api/projects", token)
    return next((p["id"] for p in projects if p.get("slug") == "showcase"), projects[0]["id"])


def create_agent() -> int:
    token = login()
    for a in req("GET", "/api/agents", token):
        if a.get("name") == AGENT_NAME:
            req("DELETE", f"/api/agents/{a['id']}", token)
    created = req(
        "POST",
        "/api/agents",
        token,
        {"name": AGENT_NAME, "labels": ["os=linux"], "concurrency": 2},
    )
    print(created["token"])
    return 0


def run_pipeline() -> int:
    token = login()
    project_id = showcase(token)
    definition = {
        "name": PIPELINE_NAME,
        "steps": [
            {
                "id": "hello",
                "name": "hello",
                "needs": [],
                # The artifact is the point as much as the echo: storing it exercises the
                # shipped default backend — the local filesystem, written by uid 10001
                # under a read-only rootfs into a Docker volume. Nothing else covers that
                # combination end to end, and it is the one that used to lose artifacts
                # into the API's log while the build stayed green.
                "run": "echo compose-smoke-ok | tee out.txt",
                "artifacts": ["out.txt"],
                "labels": ["os=linux"],
                "timeout_minutes": 5,
            }
        ],
    }
    existing = next(
        (p for p in req("GET", f"/api/projects/{project_id}/pipelines", token)
         if p["name"] == PIPELINE_NAME),
        None,
    )
    if existing:
        pipeline = req("PUT", f"/api/pipelines/{existing['id']}", token,
                       {"name": PIPELINE_NAME, "definition": definition})
    else:
        pipeline = req("POST", f"/api/projects/{project_id}/pipelines", token,
                       {"name": PIPELINE_NAME, "definition": definition})

    # Wait for the containerised agent to register before starting the run.
    for _ in range(60):
        if any(a.get("name") == AGENT_NAME and a.get("online") for a in req("GET", "/api/agents", token)):
            break
        time.sleep(1)
    else:
        print("compose agent never came online", file=sys.stderr)
        return 1

    started = req("POST", f"/api/pipelines/{pipeline['id']}/runs", token, {"trigger": "smoke"})
    run_id = (started.get("run") or started)["id"]
    for _ in range(120):
        detail = req("GET", f"/api/runs/{run_id}", token)
        status = detail["run"]["status"]
        if status != "running":
            break
        time.sleep(1)
    else:
        print(f"run {run_id} did not finish", file=sys.stderr)
        return 1

    step = detail["steps"][0]
    logs = [line["data"] for line in req("GET", f"/api/steps/{step['id']}/logs", token)]
    if status != "succeeded" or "compose-smoke-ok" not in logs:
        print(f"run={status} step={step['status']} error={step.get('error')} logs={logs[-5:]}",
              file=sys.stderr)
        return 1

    # A green step whose artifact was never stored is exactly the failure this covers,
    # so assert the row exists rather than trusting the status.
    artifacts = req("GET", f"/api/runs/{run_id}/artifacts", token)
    names = [a["name"] for a in artifacts]
    if "out.txt" not in names:
        print(f"run {run_id} succeeded but stored no out.txt: {names}", file=sys.stderr)
        return 1
    print(f"run {run_id} succeeded on the compose agent, artifacts={names}")
    return 0


RESTART_PIPELINE_NAME = "compose-smoke-restart"
RESTART_MARKER = "compose-smoke-restart-ok"


def _upsert_pipeline(token: str, project_id: str, name: str, definition: dict) -> dict:
    existing = next(
        (p for p in req("GET", f"/api/projects/{project_id}/pipelines", token)
         if p["name"] == name),
        None,
    )
    if existing:
        return req("PUT", f"/api/pipelines/{existing['id']}", token,
                   {"name": name, "definition": definition})
    return req("POST", f"/api/projects/{project_id}/pipelines", token,
               {"name": name, "definition": definition})


def restart_run_start() -> int:
    """Start a step slow enough to restart the API under, and print its run id once
    the step is running on the agent."""
    token = login()
    project_id = showcase(token)
    definition = {
        "name": RESTART_PIPELINE_NAME,
        "steps": [
            {
                "id": "slow",
                "name": "slow",
                "needs": [],
                # Long enough for the API to go down and come back while it runs.
                "run": f"for i in $(seq 1 25); do echo tick $i; sleep 1; done; echo {RESTART_MARKER}",
                "labels": ["os=linux"],
                "timeout_minutes": 5,
                "retries": 0,
            }
        ],
    }
    pipeline = _upsert_pipeline(token, project_id, RESTART_PIPELINE_NAME, definition)
    started = req("POST", f"/api/pipelines/{pipeline['id']}/runs", token, {"trigger": "smoke"})
    run_id = (started.get("run") or started)["id"]
    for _ in range(60):
        detail = req("GET", f"/api/runs/{run_id}", token)
        if detail["steps"][0]["status"] == "running":
            print(run_id)
            return 0
        if detail["run"]["status"] != "running":
            print(f"run {run_id} ended before the step ran: {detail['run']['status']}",
                  file=sys.stderr)
            return 1
        time.sleep(1)
    print(f"run {run_id}: step never started", file=sys.stderr)
    return 1


def restart_run_verify(run_id: str) -> int:
    """The step that was running across the API restart finished on its first attempt:
    the lease outlived the session, so nothing was requeued."""
    token = login()
    for _ in range(120):
        detail = req("GET", f"/api/runs/{run_id}", token)
        status = detail["run"]["status"]
        if status != "running":
            break
        time.sleep(1)
    else:
        print(f"run {run_id} did not finish after the API restart", file=sys.stderr)
        return 1
    step = detail["steps"][0]
    attempts = req("GET", f"/api/steps/{step['id']}/attempts", token)
    logs = [line["data"] for line in req("GET", f"/api/steps/{step['id']}/logs", token)]
    problems = []
    if status != "succeeded":
        problems.append(f"run={status} step={step['status']} error={step.get('error')}")
    if step.get("attempt") != 1 or len(attempts) != 1:
        problems.append(f"expected one attempt, got attempt={step.get('attempt')} attempts={attempts}")
    if RESTART_MARKER not in logs:
        problems.append(f"marker missing from logs; tail={logs[-5:]}")
    # Lines produced while the API was down were buffered on the agent and flushed
    # after the reconnect; none of the 25 ticks may be missing.
    # A line the outbox re-sent after an aborted write can appear twice (append-only,
    # no dedupe); a missing tick is the failure, a duplicate is not.
    ticks = sorted({int(l.split()[1]) for l in logs if l.startswith("tick ")})
    if ticks != list(range(1, 26)):
        problems.append(f"ticks lost across the restart: {ticks}")
    if problems:
        print("; ".join(problems), file=sys.stderr)
        return 1
    print(f"run {run_id} completed on attempt 1 across the API restart")
    return 0


def cleanup() -> int:
    token = login()
    for a in req("GET", "/api/agents", token):
        if a.get("name") == AGENT_NAME:
            req("DELETE", f"/api/agents/{a['id']}", token)
    return 0


if __name__ == "__main__":
    action = sys.argv[1] if len(sys.argv) > 1 else ""
    try:
        actions = {
            "create-agent": create_agent,
            "run-pipeline": run_pipeline,
            "restart-run-start": restart_run_start,
            "restart-run-verify": lambda: restart_run_verify(sys.argv[2]),
            "cleanup": cleanup,
        }
        sys.exit(actions[action]())
    except KeyError:
        print(f"unknown action: {action}", file=sys.stderr)
        sys.exit(2)
    except urllib.error.HTTPError as e:
        print(f"HTTP {e.code}: {e.read().decode()[:200]}", file=sys.stderr)
        sys.exit(1)
