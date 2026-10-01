#!/usr/bin/env python3
"""Smoke: run lifecycle — retry (failed-only and full), continue_on_error, a push to a
pipeline that no longer compiles, and one durable fiber.

Needs an agent already online with `os=linux` (CI's smoke-host starts one; locally see
docs/development.md), and `psql` for the one check that has to break a stored definition
the API would refuse to save.
"""
from __future__ import annotations

import hashlib
import hmac
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

# Honour both, like the other smokes: hardcoding 18080 / the dev DSN tests whatever
# happens to be there instead of the API you started.
API = os.environ.get("FIBER_API_URL", "http://127.0.0.1:18080").rstrip("/")
DSN = os.environ.get("FIBER_DATABASE_URL", "postgres://fiber:fiber@127.0.0.1:15432/fiber")
TERMINAL = ("succeeded", "failed", "cancelled")


def req(method: str, path: str, token: str | None = None, body=None, headers=None, raw: bytes | None = None):
    data = raw if raw is not None else (None if body is None else json.dumps(body).encode())
    h = {"Content-Type": "application/json"}
    if token:
        h["Authorization"] = f"Bearer {token}"
    h.update(headers or {})
    r = urllib.request.Request(API + path, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=30) as resp:
            text = resp.read().decode()
            return resp.status, (json.loads(text) if text else {})
    except urllib.error.HTTPError as e:
        text = e.read().decode()
        try:
            return e.code, json.loads(text)
        except ValueError:
            return e.code, text


def check(name: str, cond: bool, detail=None) -> None:
    if not cond:
        print(f"FAIL {name}: {detail}")
        sys.exit(1)
    print(f"OK  {name}")


def wait_run(token: str, run_id: str, timeout: float = 120.0) -> dict:
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, full = req("GET", f"/api/runs/{run_id}", token=token)
        if full["run"]["status"] in TERMINAL:
            return full
        time.sleep(1)
    raise SystemExit(f"FAIL run {run_id} did not finish in {timeout:.0f}s")


def steps_by_id(full: dict) -> dict:
    return {s["step_id"]: s for s in full["steps"]}


def log_lines(token: str, step_run_id: str) -> list:
    _, logs = req("GET", f"/api/steps/{step_run_id}/logs?limit=500", token=token)
    return logs if isinstance(logs, list) else logs.get("lines", logs.get("items", []))


def create_pipeline(token: str, pid: str, name: str, definition: dict) -> str:
    code, pipe = req("POST", f"/api/projects/{pid}/pipelines", token, {"name": name, "definition": definition})
    check(f"create pipeline {name}", code in (200, 201), pipe)
    return pipe["id"]


def start(token: str, pipeline_id: str) -> str:
    code, body = req("POST", f"/api/pipelines/{pipeline_id}/runs", token, {})
    check("start run", code in (200, 201), body)
    return body["run"]["id"]


def main() -> None:
    _, ready = req("GET", "/ready")
    check("api ready", isinstance(ready, dict) and ready.get("ok"), ready)
    _, login = req("POST", "/api/auth/login", body={"username": "admin", "password": "fiber"})
    token = login["token"]
    _, agents = req("GET", "/api/agents", token=token)
    check(
        "an agent is online",
        any(a.get("online") for a in agents),
        "start one before smoke-runs (docs/development.md); `make smoke-pools` stops every agent",
    )

    code, project = req("POST", "/api/projects", token, {"name": "Smoke runs", "slug": f"smoke-runs-{int(time.time())}"})
    check("create project", code in (200, 201), project)
    pid = project["id"]
    labels = ["os=linux"]
    try:
        # ---- retry: `b` fails the first time it runs and passes after that -------------
        marker = os.path.join(tempfile.gettempdir(), f"fiber-smoke-runs-{uuid.uuid4().hex}")
        retry_pipe = create_pipeline(token, pid, "retry", {
            "name": "retry",
            "env": {"MARK": marker},
            "steps": [
                {"id": "a", "name": "a", "needs": [], "labels": labels, "run": "echo from-a"},
                {"id": "b", "name": "b", "needs": ["a"], "labels": labels,
                 "run": 'if [ -f "$MARK" ]; then echo second-try; else touch "$MARK"; exit 3; fi'},
            ],
        })
        first = wait_run(token, start(token, retry_pipe))
        s = steps_by_id(first)
        check("first run fails at b", first["run"]["status"] == "failed" and s["b"]["status"] == "failed"
              and s["a"]["status"] == "succeeded", {k: v["status"] for k, v in s.items()})

        code, body = req("POST", f"/api/runs/{first['run']['id']}/retry", token, {"failed_only": True})
        check("retry failed steps is accepted", code in (200, 201), body)
        again = wait_run(token, body["run"]["id"])
        s2 = steps_by_id(again)
        check("failed-only retry succeeds", again["run"]["status"] == "succeeded",
              {k: v["status"] for k, v in s2.items()})
        check("it records what it retried", again["run"].get("retry_of") == first["run"]["id"], again["run"])
        check("a passing step is carried over, not re-run",
              s2["a"]["status"] == "succeeded" and not log_lines(token, s2["a"]["id"]),
              log_lines(token, s2["a"]["id"]))

        code, body = req("POST", f"/api/runs/{first['run']['id']}/retry", token, {"failed_only": False})
        full = wait_run(token, body["run"]["id"])
        s3 = steps_by_id(full)
        check("full retry runs every step again", full["run"]["status"] == "succeeded"
              and any("from-a" in l["data"] for l in log_lines(token, s3["a"]["id"])))
        if os.path.exists(marker):
            os.remove(marker)

        # ---- continue_on_error: the failure is recorded, the run and dependents go on ---
        coe_pipe = create_pipeline(token, pid, "tolerated", {
            "name": "tolerated",
            "steps": [
                {"id": "flaky", "name": "flaky", "needs": [], "labels": labels, "run": "exit 1",
                 "continue_on_error": True},
                {"id": "after", "name": "after", "needs": ["flaky"], "labels": labels, "run": "echo after"},
            ],
        })
        tolerated = wait_run(token, start(token, coe_pipe))
        s = steps_by_id(tolerated)
        check("continue_on_error: run succeeds, step failed, dependent ran",
              tolerated["run"]["status"] == "succeeded" and s["flaky"]["status"] == "failed"
              and s["after"]["status"] == "succeeded", {k: v["status"] for k, v in s.items()})

        # ---- a push to a pipeline that no longer compiles leaves a failed run ----------
        broken_pipe = create_pipeline(token, pid, "broken", {
            "name": "broken",
            "on": {"push": {"branches": ["main"]}},
            "steps": [{"id": "x", "name": "x", "needs": [], "labels": labels, "run": "echo x"}],
        })
        # The API refuses to save a definition like this, which is the point: it stands
        # for one that compiled under an older version and no longer does.
        stored = json.loads(subprocess.run(
            ["psql", DSN, "-Atc", f"SELECT definition FROM pipelines WHERE id = '{broken_pipe}'"],
            capture_output=True, text=True, check=True).stdout)
        steps = stored["steps"]
        (steps[0] if isinstance(steps, list) else next(iter(steps.values())))["shell"] = "-x"
        subprocess.run(["psql", DSN, "-qc",
                        f"UPDATE pipelines SET definition = '{json.dumps(stored)}'::jsonb WHERE id = '{broken_pipe}'"],
                       check=True)
        secret = "smoke-runs-secret"
        req("PUT", f"/api/projects/{pid}/webhooks/github", token, {"secret": secret})
        payload = json.dumps({"ref": "refs/heads/main", "after": "a" * 40, "commits": [],
                              "head_commit": {"added": [], "modified": ["x"], "removed": []}}).encode()
        sig = "sha256=" + hmac.new(secret.encode(), payload, hashlib.sha256).hexdigest()
        code, body = req("POST", f"/api/projects/{pid}/webhooks/github", raw=payload,
                         headers={"X-GitHub-Event": "push", "X-Hub-Signature-256": sig})
        failed = (body.get("failed") or [{}])[0] if isinstance(body, dict) else {}
        check("the push reports the pipeline as failed, with a run", bool(failed.get("run")), body)
        _, full = req("GET", f"/api/runs/{failed['run']}", token=token)
        run = full["run"]
        check("that run is failed, has no steps, and says why",
              run["status"] == "failed" and not full["steps"] and "shell" in (run.get("error") or ""), run)
        code, body = req("POST", f"/api/runs/{run['id']}/retry", token, {})
        check("retrying it is refused", code == 400, (code, body))
        code, body = req("POST", f"/api/pipelines/{broken_pipe}/runs", token, {})
        check("a manual start still gets 400", code == 400, (code, body))

        # ---- one durable fiber runs to completion ------------------------------------
        code, fiber = req("POST", f"/api/projects/{pid}/fibers", token, {"name": "ping", "input": {}})
        check("start a ping fiber", code in (200, 201), fiber)
        fid = fiber.get("id") or fiber.get("fiber", {}).get("id")
        deadline, status = time.time() + 60, None
        while time.time() < deadline:
            _, f = req("GET", f"/api/fibers/{fid}", token=token)
            status = (f.get("fiber") or f).get("status")
            if status in ("completed", "succeeded", "failed", "cancelled"):
                break
            time.sleep(1)
        check("the fiber completes", status in ("completed", "succeeded"), status)
    finally:
        req("DELETE", f"/api/projects/{pid}", token)
    print("SMOKE_OK runs")


if __name__ == "__main__":
    main()
