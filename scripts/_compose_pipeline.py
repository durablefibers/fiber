#!/usr/bin/env python3
"""Helpers for dogfood_compose.sh: mint an agent, run one pipeline, clean up."""
from __future__ import annotations

import json
import sys
import time
import urllib.error
import urllib.request

API = "http://127.0.0.1:18080"
AGENT_NAME = "compose-dogfood"
PIPELINE_NAME = "compose-dogfood"


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
                "run": "echo compose-dogfood-ok",
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

    started = req("POST", f"/api/pipelines/{pipeline['id']}/runs", token, {"trigger": "dogfood"})
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
    if status != "succeeded" or "compose-dogfood-ok" not in logs:
        print(f"run={status} step={step['status']} error={step.get('error')} logs={logs[-5:]}",
              file=sys.stderr)
        return 1
    print(f"run {run_id} succeeded on the compose agent")
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
        sys.exit({"create-agent": create_agent, "run-pipeline": run_pipeline, "cleanup": cleanup}[action]())
    except KeyError:
        print(f"unknown action: {action}", file=sys.stderr)
        sys.exit(2)
    except urllib.error.HTTPError as e:
        print(f"HTTP {e.code}: {e.read().decode()[:200]}", file=sys.stderr)
        sys.exit(1)
