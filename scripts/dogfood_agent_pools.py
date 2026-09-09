#!/usr/bin/env python3
"""Smoke: project-scoped agents only lease that project's steps; globals can take any."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

# Honour FIBER_API_URL. Hardcoding this made the script silently test whatever was on
# 18080 — which, when you are running a second API on another port to check a change,
# is the old build, and the smoke passes without having tested anything you wrote.
API = os.environ.get("FIBER_API_URL", "http://127.0.0.1:18080").rstrip("/")
FAILS = 0
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))


def req(method: str, path: str, token: str | None = None, body: dict | None = None):
    data = None if body is None else json.dumps(body).encode()
    h = {"Content-Type": "application/json"}
    if token:
        h["Authorization"] = f"Bearer {token}"
    r = urllib.request.Request(API + path, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=30) as resp:
            raw = resp.read().decode()
            return resp.status, json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            payload = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            payload = {"error": raw}
        return e.code, payload


def check(name: str, cond: bool, detail: object = None) -> None:
    global FAILS
    if cond:
        print(f"OK  {name}")
    else:
        FAILS += 1
        print(f"FAIL {name}: {detail}")


def wait_run(token: str, run_id: str, want: str, timeout: float = 45.0) -> dict:
    deadline = time.time() + timeout
    last = {}
    while time.time() < deadline:
        code, run = req("GET", f"/api/runs/{run_id}", token=token)
        if code == 200:
            last = run
            status = run.get("status") or run.get("run", {}).get("status")
            if status == want:
                return run
            if status in ("failed", "cancelled") and want == "succeeded":
                return run
        time.sleep(0.5)
    return last


def start_agent(token: str, name: str, log_path: str) -> subprocess.Popen:
    env = os.environ.copy()
    env.update(
        {
            "FIBER_AGENT_TOKEN": token,
            "FIBER_API_URL": "ws://127.0.0.1:18080",
            "FIBER_AGENT_USE_DOCKER": "false",
            "FIBER_AGENT_LABELS": "os=linux,pool=dogfood",
            "FIBER_AGENT_NAME": name,
            "FIBER_AGENT_WORKSPACE_DIR": os.path.join(ROOT, "data", "workspaces"),
        }
    )
    log = open(log_path, "w")
    return subprocess.Popen(
        [os.path.join(ROOT, "target/debug/fiber-agent")],
        cwd=ROOT,
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
    )


def main() -> int:
    code, ready = req("GET", "/ready")
    check("ready", code == 200 and ready.get("ok") is True, ready)
    if FAILS:
        return 1

    code, login = req("POST", "/api/auth/login", body={"username": "admin", "password": "fiber"})
    check("login", code == 200 and "token" in login, login)
    admin = login["token"]

    # Two projects
    code, pa = req(
        "POST",
        "/api/projects",
        token=admin,
        body={"name": "Pool A", "slug": f"pool-a-{int(time.time())}"},
    )
    check("create project A", code in (200, 201) and "id" in pa, pa)
    code, pb = req(
        "POST",
        "/api/projects",
        token=admin,
        body={"name": "Pool B", "slug": f"pool-b-{int(time.time())}"},
    )
    check("create project B", code in (200, 201) and "id" in pb, pb)
    pid_a, pid_b = pa["id"], pb["id"]

    def_pipe = {
        "name": "pool-probe",
        "steps": [
            {
                "id": "hi",
                "name": "hi",
                "needs": [],
                "run": "echo pool-ok",
                "labels": ["os=linux", "pool=dogfood"],
            }
        ],
    }
    code, pipe_a = req(
        "POST",
        f"/api/projects/{pid_a}/pipelines",
        token=admin,
        body={"name": "pool-a-pipe", "definition": def_pipe},
    )
    check("pipeline A", code in (200, 201) and "id" in pipe_a, pipe_a)
    code, pipe_b = req(
        "POST",
        f"/api/projects/{pid_b}/pipelines",
        token=admin,
        body={"name": "pool-b-pipe", "definition": def_pipe},
    )
    check("pipeline B", code in (200, 201) and "id" in pipe_b, pipe_b)

    # Scoped agent for A only
    code, scoped = req(
        "POST",
        "/api/agents",
        token=admin,
        body={
            "name": "scoped-a",
            "labels": ["os=linux", "pool=dogfood"],
            "concurrency": 1,
            "project_id": pid_a,
        },
    )
    check(
        "create scoped agent",
        code == 201 and scoped.get("agent", {}).get("project_id") == pid_a,
        scoped,
    )
    scoped_tok = scoped["token"]
    scoped_id = scoped["agent"]["id"]

    code, listed = req("GET", f"/api/agents?project_id={pid_a}", token=admin)
    check(
        "list filter includes scoped",
        code == 200 and any(a.get("id") == scoped_id for a in listed),
        listed,
    )

    # Prefer killing only the agent binary (avoid matching shells that mention fiber-agent).
    try:
        out = subprocess.check_output(["pgrep", "-fl", "fiber-agent"], text=True)
        for line in out.splitlines():
            if "/fiber-agent" not in line and not line.rstrip().endswith("fiber-agent"):
                continue
            if "fiber-api" in line or "pkill" in line or "pgrep" in line:
                continue
            pid = int(line.split(None, 1)[0])
            try:
                os.kill(pid, 15)
            except OSError:
                pass
    except subprocess.CalledProcessError:
        pass
    time.sleep(0.5)

    proc = start_agent(scoped_tok, "scoped-a", "/tmp/fiber-agent-pool-scoped.log")
    time.sleep(1.5)

    code, run_a = req("POST", f"/api/pipelines/{pipe_a['id']}/runs", token=admin, body={})
    check("start run A", code in (200, 201) and "run" in run_a, run_a)
    rid_a = run_a.get("run", {}).get("id") if isinstance(run_a, dict) else None
    finished_a = wait_run(admin, rid_a, "succeeded", timeout=30) if rid_a else {}
    check("scoped agent runs project A", finished_a.get("status") == "succeeded" or finished_a.get("run", {}).get("status") == "succeeded", finished_a)

    code, run_b = req("POST", f"/api/pipelines/{pipe_b['id']}/runs", token=admin, body={})
    check("start run B", code in (200, 201) and "run" in run_b, run_b)
    rid_b = run_b.get("run", {}).get("id")
    # Should stay pending/queued — scoped agent must not pick it up
    time.sleep(4)
    code, mid_b = req("GET", f"/api/runs/{rid_b}", token=admin)
    mid_status = mid_b.get("status") or mid_b.get("run", {}).get("status")
    check(
        "scoped agent ignores project B",
        code == 200 and mid_status in ("pending", "running"),
        mid_b,
    )
    code, steps_b = req("GET", f"/api/runs/{rid_b}/steps", token=admin)
    if isinstance(steps_b, dict) and "steps" in steps_b:
        steps_b = steps_b["steps"]
    step_statuses = [s.get("status") for s in steps_b] if isinstance(steps_b, list) else []
    check(
        "B steps still queued (not leased by scoped)",
        all(s in ("queued", "pending") for s in step_statuses) and len(step_statuses) > 0,
        step_statuses,
    )

    # Global agent finishes B
    code, glob = req(
        "POST",
        "/api/agents",
        token=admin,
        body={
            "name": "global-pool",
            "labels": ["os=linux", "pool=dogfood"],
            "concurrency": 1,
        },
    )
    check(
        "create global agent",
        code == 201 and glob.get("agent", {}).get("project_id") in (None, ""),
        glob,
    )
    glob_proc = start_agent(glob["token"], "global-pool", "/tmp/fiber-agent-pool-global.log")
    time.sleep(1.5)
    finished_b = wait_run(admin, rid_b, "succeeded", timeout=30)
    b_status = finished_b.get("status") or finished_b.get("run", {}).get("status")
    check("global agent runs project B", b_status == "succeeded", finished_b)

    # Cleanup
    for p in (proc, glob_proc):
        try:
            p.terminate()
            p.wait(timeout=5)
        except Exception:
            try:
                p.kill()
            except Exception:
                pass
    req("DELETE", f"/api/agents/{scoped_id}", token=admin)
    req("DELETE", f"/api/agents/{glob['agent']['id']}", token=admin)

    print("---")
    if FAILS:
        print(f"DOGFOOD_FAIL failures={FAILS}")
        return 1
    print("DOGFOOD_OK agent-pools")
    return 0


if __name__ == "__main__":
    sys.exit(main())
