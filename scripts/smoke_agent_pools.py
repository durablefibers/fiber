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
# The agent dials the same host over WebSocket. Derived, not hardcoded: a hardcoded
# ws://127.0.0.1:18080 pointed the agents at whatever was on 18080 while the assertions
# talked to $FIBER_API_URL, so in CI — or against a second API on another port — the
# smoke tested two different servers and still passed.
WS = API.replace("https://", "wss://", 1).replace("http://", "ws://", 1)
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


def wait_online(token: str, agent_id: str, timeout: float = 30.0) -> bool:
    """Poll until the API reports the agent online.

    A fixed sleep here was a bet on how long registration takes: too short and the run
    starts before any agent can take it (a flake that looks like a scheduling bug), too
    long and every smoke pays for the worst case.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        code, agents = req("GET", "/api/agents", token=token)
        if code == 200 and isinstance(agents, list):
            if any(a.get("id") == agent_id and a.get("online") for a in agents):
                return True
        time.sleep(0.25)
    return False


def holds(predicate, seconds: float, interval: float = 0.5) -> tuple[bool, object]:
    """Assert a *negative* — that something does not happen — by re-checking it.

    `time.sleep(4); check(...)` samples once and calls the other 3.5 seconds proof. This
    evaluates the invariant throughout the window and returns the first violation, so a
    scoped agent that leases the wrong project's step for half a second is caught rather
    than slept through.
    """
    deadline = time.time() + seconds
    detail: object = None
    while time.time() < deadline:
        ok, detail = predicate()
        if not ok:
            return False, detail
        time.sleep(interval)
    return True, detail


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
            "FIBER_API_URL": WS,
            "FIBER_AGENT_USE_DOCKER": "false",
            "FIBER_AGENT_LABELS": "os=linux,pool=smoke",
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


def drop_projects(token: str, project_ids: list[str]) -> None:
    """Delete the projects this run created, so repeated smokes do not pile up.

    Only on a pass. A failed smoke leaves them behind on purpose: the project is the
    only record of what happened, and deleting it takes the pipelines, runs, logs and
    artifacts needed to work out why with it.
    """
    for pid in project_ids:
        code, body = req("DELETE", f"/api/projects/{pid}", token=token)
        if code == 200:
            print(f"cleaned up project {pid}")
        else:
            print(f"WARN  could not delete project {pid}: {code} {body}")


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
                "labels": ["os=linux", "pool=smoke"],
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
            "labels": ["os=linux", "pool=smoke"],
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
    check("scoped agent online", wait_online(admin, scoped_id), "never came online")

    code, run_a = req("POST", f"/api/pipelines/{pipe_a['id']}/runs", token=admin, body={})
    check("start run A", code in (200, 201) and "run" in run_a, run_a)
    rid_a = run_a.get("run", {}).get("id") if isinstance(run_a, dict) else None
    finished_a = wait_run(admin, rid_a, "succeeded", timeout=30) if rid_a else {}
    check("scoped agent runs project A", finished_a.get("status") == "succeeded" or finished_a.get("run", {}).get("status") == "succeeded", finished_a)

    code, run_b = req("POST", f"/api/pipelines/{pipe_b['id']}/runs", token=admin, body={})
    check("start run B", code in (200, 201) and "run" in run_b, run_b)
    rid_b = run_b.get("run", {}).get("id")

    # Negative assertion: the only online agent is scoped to project A, so B's step must
    # stay queued. The window covers more than one agent heartbeat (10 s is the offer
    # interval; every offer point in between is also exercised), and a violation fails
    # the instant it happens instead of after the window.
    def b_untouched() -> tuple[bool, object]:
        code, run = req("GET", f"/api/runs/{rid_b}", token=admin)
        if code != 200:
            return False, run
        status = run.get("status") or run.get("run", {}).get("status")
        if status not in ("pending", "running"):
            return False, run
        code, steps = req("GET", f"/api/runs/{rid_b}/steps", token=admin)
        if isinstance(steps, dict) and "steps" in steps:
            steps = steps["steps"]
        statuses = [s.get("status") for s in steps] if isinstance(steps, list) else []
        if not statuses:
            return False, "run B has no steps"
        if not all(s in ("queued", "pending") for s in statuses):
            return False, statuses
        return True, statuses

    ok, detail = holds(b_untouched, seconds=12.0)
    check("scoped agent ignores project B for a full offer cycle", ok, detail)

    # Global agent finishes B
    code, glob = req(
        "POST",
        "/api/agents",
        token=admin,
        body={
            "name": "global-pool",
            "labels": ["os=linux", "pool=smoke"],
            "concurrency": 1,
        },
    )
    check(
        "create global agent",
        code == 201 and glob.get("agent", {}).get("project_id") in (None, ""),
        glob,
    )
    glob_proc = start_agent(glob["token"], "global-pool", "/tmp/fiber-agent-pool-global.log")
    check(
        "global agent online",
        wait_online(admin, glob["agent"]["id"]),
        "never came online",
    )
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
        print(f"SMOKE_FAIL failures={FAILS}")
        print(f"kept projects for inspection: {pid_a} {pid_b}")
        return 1
    drop_projects(admin, [pid_a, pid_b])
    print("SMOKE_OK agent-pools")
    return 0


if __name__ == "__main__":
    sys.exit(main())
