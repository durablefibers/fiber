#!/usr/bin/env python3
"""Smoke: concurrency groups and guarded cancel, end to end.

- A second run in a `cancel_in_progress` group leaves exactly one run in flight and ends
  the older one `cancelled`, its step carrying `superseded by a newer run`.
- A retry is a run in the same group, so it supersedes too.
- A cancel that arrives after a run finished changes nothing: it stays `succeeded`.
- An agent fills every free slot in one heartbeat: three parallel root steps on a
  concurrency-3 agent are all running within seconds, not one per 10-second heartbeat.
- A project whose secret cannot be decrypted does not block the queue: its step ends
  `failed` with the decrypt reason, and a run in a healthy project still leases and
  completes behind it. (Poisoned through the API by storing a literal `enc:v1:…` value,
  which only decrypts to nothing when the API has no `FIBER_SECRETS_KEY` — the dev
  default; with a key set the literal round-trips and the scenario reports SKIP.)

Needs infra and a rebuilt fiber-api; starts (and stops) its own fiber-agent.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

# Honour FIBER_API_URL so a second API on another port tests the build you are checking,
# not whatever is on 18080. The agent connects to the same host over WS.
API = os.environ.get("FIBER_API_URL", "http://127.0.0.1:18080").rstrip("/")
WS = API.replace("https://", "wss://", 1).replace("http://", "ws://", 1)
FAILS = 0
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
LABELS = ["os=linux", "pool=smoke-concurrency"]
SUPERSEDED = "superseded by a newer run"


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


def run_status(token: str, run_id: str) -> str | None:
    code, run = req("GET", f"/api/runs/{run_id}", token=token)
    if code != 200:
        return None
    return run.get("status") or run.get("run", {}).get("status")


def steps_of(token: str, run_id: str) -> list[dict]:
    code, steps = req("GET", f"/api/runs/{run_id}/steps", token=token)
    if isinstance(steps, dict) and "steps" in steps:
        steps = steps["steps"]
    return steps if code == 200 and isinstance(steps, list) else []


def wait_for(what: str, pred, timeout: float = 30.0, every: float = 0.25):
    """Poll `pred` until it returns a truthy value; never sleep for a negative check."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = pred()
        if got:
            return got
        time.sleep(every)
    return None


def wait_run_status(token: str, run_id: str, want: str, timeout: float = 30.0) -> str | None:
    terminal = {"succeeded", "failed", "cancelled"}
    last = {"s": None}

    def done():
        s = run_status(token, run_id)
        last["s"] = s
        return s == want or (s in terminal and want in terminal)

    wait_for(f"run {run_id} -> {want}", done, timeout)
    return last["s"]


def wait_step_status(token: str, run_id: str, want: str, timeout: float = 30.0) -> str | None:
    last = {"s": None}

    def done():
        steps = steps_of(token, run_id)
        s = steps[0].get("status") if steps else None
        last["s"] = s
        return s == want

    wait_for(f"step of {run_id} -> {want}", done, timeout)
    return last["s"]


def start_agent(token: str, name: str, log_path: str, concurrency: int) -> subprocess.Popen:
    env = os.environ.copy()
    env.update(
        {
            "FIBER_AGENT_TOKEN": token,
            "FIBER_API_URL": WS,
            "FIBER_AGENT_USE_DOCKER": "false",
            "FIBER_AGENT_LABELS": ",".join(LABELS),
            "FIBER_AGENT_NAME": name,
            "FIBER_AGENT_CONCURRENCY": str(concurrency),
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


def stop_agent(proc: subprocess.Popen) -> None:
    try:
        proc.terminate()
        proc.wait(timeout=10)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass


def drop_project(token: str, project_id: str) -> None:
    """Only on a pass: a failed smoke keeps its project, which is the evidence."""
    code, body = req("DELETE", f"/api/projects/{project_id}", token=token)
    if code == 200:
        print(f"cleaned up project {project_id}")
    else:
        print(f"WARN  could not delete project {project_id}: {code} {body}")


def main() -> int:
    code, ready = req("GET", "/ready")
    check("ready", code == 200 and ready.get("ok") is True, ready)
    if FAILS:
        return 1

    code, login = req("POST", "/api/auth/login", body={"username": "admin", "password": "fiber"})
    check("login", code == 200 and "token" in login, login)
    admin = login["token"]

    code, proj = req(
        "POST",
        "/api/projects",
        token=admin,
        body={"name": "Concurrency", "slug": f"concurrency-{int(time.time())}"},
    )
    check("create project", code in (200, 201) and "id" in proj, proj)
    pid = proj["id"]

    # A group that cancels, with a step long enough to still be running when the next
    # run starts — and short enough that a missed cancel does not hold the smoke.
    grouped = {
        "name": "group-probe",
        "concurrency": {"group": "smoke-{pipeline}", "cancel_in_progress": True},
        "steps": [
            {"id": "work", "name": "work", "needs": [], "run": "sleep 40", "labels": LABELS}
        ],
    }
    fan = {
        "name": "fan-probe",
        "steps": [
            {"id": f"w{i}", "name": f"w{i}", "needs": [], "run": "sleep 30", "labels": LABELS}
            for i in range(3)
        ],
    }
    plain = {
        "name": "plain-probe",
        "steps": [
            {"id": "hi", "name": "hi", "needs": [], "run": "echo done", "labels": LABELS}
        ],
    }
    code, pipe_g = req(
        "POST",
        f"/api/projects/{pid}/pipelines",
        token=admin,
        body={"name": "group-probe", "definition": grouped},
    )
    check("grouped pipeline", code in (200, 201) and "id" in pipe_g, pipe_g)
    code, pipe_p = req(
        "POST",
        f"/api/projects/{pid}/pipelines",
        token=admin,
        body={"name": "plain-probe", "definition": plain},
    )
    check("plain pipeline", code in (200, 201) and "id" in pipe_p, pipe_p)
    code, pipe_f = req(
        "POST",
        f"/api/projects/{pid}/pipelines",
        token=admin,
        body={"name": "fan-probe", "definition": fan},
    )
    check("fan pipeline", code in (200, 201) and "id" in pipe_f, pipe_f)

    # Concurrency 3: if the group did not cancel, its runs *could* run side by side, so
    # "exactly one in flight" is the group's doing and not the agent's cap — and the
    # fan-out check needs every slot. Global (no project), so the poisoned project's
    # step and the healthy one share this agent's queue: that is where a head-of-line
    # block would show. The labels keep it from taking anything else on the instance.
    code, agent = req(
        "POST",
        "/api/agents",
        token=admin,
        body={"name": "smoke-concurrency", "labels": LABELS, "concurrency": 3},
    )
    check("create agent", code == 201 and "token" in agent, agent)
    agent_id = agent["agent"]["id"]
    proc = start_agent(agent["token"], "smoke-concurrency", "/tmp/fiber-agent-smoke-concurrency.log", 3)
    projects_to_drop = [pid]

    try:
        # --- every free slot fills in one heartbeat ------------------------------------
        started = time.time()
        code, rf = req("POST", f"/api/pipelines/{pipe_f['id']}/runs", token=admin, body={})
        check("start fan run", code in (200, 201) and "run" in rf, rf)
        ridf = rf["run"]["id"]
        all_running = wait_for(
            "fan steps running",
            lambda: all(s.get("status") == "running" for s in steps_of(admin, ridf))
            and len(steps_of(admin, ridf)) == 3,
            timeout=15,
        )
        took = time.time() - started
        # One offer per 10 s heartbeat would put the third step past 20 s.
        check("three root steps lease within one heartbeat", bool(all_running) and took < 8, f"{took:.1f}s")
        code, cf = req("POST", f"/api/runs/{ridf}/cancel", token=admin)
        check("cancel fan run", code == 200 and cf.get("status") == "cancelled", cf)
        # The agent's three slots must come back once the rows leave `running`, or the
        # rest of this smoke would starve.
        freed = wait_for(
            "fan steps cancelled",
            lambda: all(s.get("status") == "cancelled" for s in steps_of(admin, ridf)),
            timeout=10,
        )
        check("fan steps are all cancelled", bool(freed), steps_of(admin, ridf))

        # --- a newer run supersedes the older one ---------------------------------------
        code, r1 = req("POST", f"/api/pipelines/{pipe_g['id']}/runs", token=admin, body={})
        check("start run 1", code in (200, 201) and "run" in r1, r1)
        rid1 = r1["run"]["id"]
        check(
            "run 1 carries its concurrency group",
            r1["run"].get("concurrency_group") == f"smoke-{pipe_g['id']}",
            r1["run"].get("concurrency_group"),
        )
        s1 = wait_step_status(admin, rid1, "running", timeout=30)
        check("run 1 step is running on the agent", s1 == "running", s1)

        code, r2 = req("POST", f"/api/pipelines/{pipe_g['id']}/runs", token=admin, body={})
        check("start run 2", code in (200, 201) and "run" in r2, r2)
        rid2 = r2["run"]["id"]

        st1 = wait_run_status(admin, rid1, "cancelled", timeout=20)
        check("run 1 ends cancelled once run 2 starts", st1 == "cancelled", st1)
        step1 = steps_of(admin, rid1)
        check(
            "run 1 step is cancelled with the superseded reason",
            bool(step1) and step1[0].get("status") == "cancelled" and step1[0].get("error") == SUPERSEDED,
            step1,
        )
        if step1:
            code, attempts = req("GET", f"/api/steps/{step1[0]['id']}/attempts", token=admin)
            check(
                "run 1's attempt is closed as cancelled with the reason",
                code == 200
                and attempts
                and attempts[-1].get("status") == "cancelled"
                and attempts[-1].get("error") == SUPERSEDED
                and attempts[-1].get("finished_at"),
                attempts,
            )
        st2 = run_status(admin, rid2)
        check("run 2 is the one left in flight", st2 in ("pending", "running"), st2)
        code, listed = req("GET", f"/api/projects/{pid}/runs", token=admin)
        items = listed.get("items", listed) if isinstance(listed, dict) else listed
        open_runs = [r["id"] for r in items if r.get("status") in ("pending", "running")]
        check("exactly one run of the group is in flight", open_runs == [rid2], open_runs)

        # --- a retry is a run in the group too --------------------------------------------
        s2 = wait_step_status(admin, rid2, "running", timeout=30)
        check("run 2 step is running on the agent", s2 == "running", s2)
        code, r4 = req("POST", f"/api/runs/{rid1}/retry", token=admin, body={})
        check("retry run 1", code in (200, 201) and "run" in r4, r4)
        rid4 = r4["run"]["id"]
        st2 = wait_run_status(admin, rid2, "cancelled", timeout=20)
        check("retrying run 1 supersedes run 2", st2 == "cancelled", st2)
        step2 = steps_of(admin, rid2)
        check(
            "run 2 step carries the superseded reason",
            bool(step2) and step2[0].get("error") == SUPERSEDED,
            step2,
        )
        st4 = run_status(admin, rid4)
        check("the retry is the one left in flight", st4 in ("pending", "running"), st4)

        # Cancelling an in-flight run is the ordinary path: it must actually cancel.
        code, cancelled = req("POST", f"/api/runs/{rid4}/cancel", token=admin)
        check("cancel the retry", code == 200 and cancelled.get("status") == "cancelled", cancelled)

        # --- an undecryptable secret fails its step and blocks nothing else --------------
        code, poisoned = req(
            "POST",
            "/api/projects",
            token=admin,
            body={"name": "Poisoned", "slug": f"poisoned-{int(time.time())}"},
        )
        check("create poisoned project", code in (200, 201) and "id" in poisoned, poisoned)
        pid_x = poisoned["id"]
        code, sec = req(
            "POST",
            f"/api/projects/{pid_x}/secrets",
            token=admin,
            body={"key": "API_KEY", "value": "enc:v1:00112233445566778899aabbccddeeff00112233"},
        )
        check("store the poisoned secret", code in (200, 201), sec)
        code, pipe_x = req(
            "POST",
            f"/api/projects/{pid_x}/pipelines",
            token=admin,
            body={"name": "poisoned-probe", "definition": plain},
        )
        check("poisoned pipeline", code in (200, 201) and "id" in pipe_x, pipe_x)
        code, rx = req("POST", f"/api/pipelines/{pipe_x['id']}/runs", token=admin, body={})
        check("start poisoned run", code in (200, 201) and "run" in rx, rx)
        ridx = rx["run"]["id"]
        code, rh = req("POST", f"/api/pipelines/{pipe_p['id']}/runs", token=admin, body={})
        check("start healthy run behind it", code in (200, 201) and "run" in rh, rh)
        ridh = rh["run"]["id"]
        sth = wait_run_status(admin, ridh, "succeeded", timeout=45)
        check("healthy run completes behind the poisoned one", sth == "succeeded", sth)
        stx = wait_run_status(admin, ridx, "failed", timeout=45)
        stepx = steps_of(admin, ridx)
        errx = stepx[0].get("error") if stepx else None
        if stx == "succeeded":
            print("SKIP poisoned-secret scenario: the API decrypted the literal enc:v1: value "
                  "(FIBER_SECRETS_KEY is set); poison the row via SQL to exercise it")
        else:
            check("poisoned run fails", stx == "failed", stx)
            check(
                "poisoned step carries the decrypt reason",
                bool(errx) and "cannot decrypt project secret API_KEY" in errx
                and "FIBER_SECRETS_KEY" in errx,
                errx,
            )
            if stepx:
                code, attx = req("GET", f"/api/steps/{stepx[0]['id']}/attempts", token=admin)
                check(
                    "poisoned step has exactly one attempt, closed with the reason",
                    code == 200 and len(attx) == 1 and attx[0].get("status") == "failed"
                    and attx[0].get("finished_at") and attx[0].get("error") == errx,
                    attx,
                )
        projects_to_drop.append(pid_x)

        # --- a cancel after a finish changes nothing --------------------------------------
        code, r3 = req("POST", f"/api/pipelines/{pipe_p['id']}/runs", token=admin, body={})
        check("start plain run", code in (200, 201) and "run" in r3, r3)
        rid3 = r3["run"]["id"]
        st3 = wait_run_status(admin, rid3, "succeeded", timeout=30)
        check("plain run succeeds", st3 == "succeeded", st3)
        code, after = req("POST", f"/api/runs/{rid3}/cancel", token=admin)
        check(
            "cancel after finish returns the run untouched",
            code == 200 and after.get("status") == "succeeded",
            after,
        )
        check("run is still succeeded afterwards", run_status(admin, rid3) == "succeeded")
        step3 = steps_of(admin, rid3)
        check(
            "its step is still succeeded afterwards",
            bool(step3) and step3[0].get("status") == "succeeded",
            step3,
        )
    finally:
        stop_agent(proc)
        req("DELETE", f"/api/agents/{agent_id}", token=admin)

    print("---")
    if FAILS:
        print(f"SMOKE_FAIL failures={FAILS}")
        print(f"kept project for inspection: {pid}")
        return 1
    for p in projects_to_drop:
        drop_project(admin, p)
    print("SMOKE_OK concurrency")
    return 0


if __name__ == "__main__":
    sys.exit(main())
