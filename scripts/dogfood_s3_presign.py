#!/usr/bin/env python3
"""Dogfood S3/MinIO artifact presign: upload via PUT URL, restore next step, s3:// paths."""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "http://127.0.0.1:18080"
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
WS_DIR = os.path.join(ROOT, "data", "workspaces")
FAILS = 0


class NoAuthRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Follow redirects without forwarding Authorization (breaks MinIO SigV4)."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None and new.has_header("Authorization"):
            new.remove_header("Authorization")
        return new


def req(method: str, path: str, token: str | None = None, body: dict | None = None):
    data = None if body is None else json.dumps(body).encode()
    h = {"Content-Type": "application/json"}
    if token:
        h["Authorization"] = f"Bearer {token}"
    r = urllib.request.Request(API + path, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=60) as resp:
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


def start_agent(agent_token: str) -> subprocess.Popen:
    env = os.environ.copy()
    env.update(
        {
            "FIBER_AGENT_TOKEN": agent_token,
            "FIBER_API_URL": "ws://127.0.0.1:18080",
            "FIBER_AGENT_USE_DOCKER": "false",
            "FIBER_AGENT_LABELS": "os=linux",
            "FIBER_AGENT_NAME": "s3-dogfood",
            "FIBER_AGENT_WORKSPACE_DIR": WS_DIR,
        }
    )
    log = open("/tmp/fiber-agent-s3-dogfood.log", "w")
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

    # Prefer a fresh project so we control labels/pipeline; fall back to showcase release.
    code, proj = req(
        "POST",
        "/api/projects",
        token=admin,
        body={"name": "S3 Dogfood", "slug": f"s3-dogfood-{int(time.time())}"},
    )
    check("create project", code in (200, 201) and "id" in proj, proj)
    pid = proj["id"]

    definition = {
        "name": "s3-artifacts",
        "steps": [
            {
                "id": "produce",
                "name": "produce",
                "needs": [],
                "run": "mkdir -p out && printf 's3-marker\\n' > out/VERSION && tar -cf out/bundle.tar out/VERSION && ls -la out",
                "labels": ["os=linux"],
                "artifacts": ["out/VERSION", "out/bundle.tar"],
            },
            {
                "id": "wait",
                "name": "wait",
                "needs": ["produce"],
                "run": "sleep 4 && echo wipe-window",
                "labels": ["os=linux"],
            },
            {
                "id": "consume",
                "name": "consume",
                "needs": ["wait"],
                "run": "test -f out/VERSION && test -f out/bundle.tar && grep -q s3-marker out/VERSION && echo restore-ok",
                "labels": ["os=linux"],
            },
        ],
    }
    code, pipe = req(
        "POST",
        f"/api/projects/{pid}/pipelines",
        token=admin,
        body={"name": "s3-artifacts", "definition": definition},
    )
    check("create pipeline", code in (200, 201) and "id" in pipe, pipe)

    code, agent = req(
        "POST",
        "/api/agents",
        token=admin,
        body={
            "name": "s3-dogfood",
            "labels": ["os=linux"],
            "concurrency": 1,
            "project_id": pid,
        },
    )
    check("create agent", code == 201 and "token" in agent, agent)
    agent_tok = agent["token"]
    agent_id = agent["agent"]["id"]

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
    proc = start_agent(agent_tok)
    time.sleep(1.5)

    code, started = req(
        "POST",
        f"/api/pipelines/{pipe['id']}/runs",
        token=admin,
        body={"trigger": "dogfood:s3"},
    )
    check("start run", code in (200, 201) and "run" in started, started)
    rid = started["run"]["id"]

    wiped = False
    status = "pending"
    final = {}
    for i in range(90):
        time.sleep(0.4)
        code, run = req("GET", f"/api/runs/{rid}", token=admin)
        if code != 200:
            continue
        final = run
        status = run.get("run", {}).get("status", "pending")
        steps = run.get("steps") or []
        summary = " ".join(f"{s['step_id']}={s['status']}" for s in steps)
        print(f"t={i} run={status} {summary}")
        produce_ok = any(
            s.get("step_id") == "produce" and s.get("status") == "succeeded" for s in steps
        )
        wait_active = any(
            s.get("step_id") == "wait" and s.get("status") in ("queued", "running", "succeeded")
            for s in steps
        )
        # Wipe during/after wait so consume must restore from S3 (not leftover files).
        if produce_ok and wait_active and not wiped:
            w = os.path.join(WS_DIR, rid)
            if os.path.isdir(w):
                shutil.rmtree(w)
                print("WIPED workspace", w)
            wiped = True
        if status in ("succeeded", "failed", "cancelled"):
            break

    check("run succeeded", status == "succeeded", final)
    check("wiped workspace before consume", wiped, "missed wipe window")

    code, arts = req("GET", f"/api/runs/{rid}/artifacts", token=admin)
    if isinstance(arts, dict) and "artifacts" in arts:
        arts = arts["artifacts"]
    check("has artifacts", isinstance(arts, list) and len(arts) >= 2, arts)

    # Public list omits storage path; confirm via DB + download redirect to MinIO.
    try:
        import subprocess as sp

        row = sp.check_output(
            [
                "psql",
                "postgres://fiber:fiber@127.0.0.1:15432/fiber",
                "-Atc",
                f"SELECT path FROM artifacts WHERE run_id = '{rid}' LIMIT 1",
            ],
            text=True,
        ).strip()
        check("db path is s3://", row.startswith("s3://fiber-artifacts/"), row)
    except Exception as e:
        check("db path is s3://", False, e)

    # Step logs prove agent used presign (system lines go to run logs, not agent stdout).
    produce_id = next(
        (s["id"] for s in (final.get("steps") or []) if s.get("step_id") == "produce"),
        None,
    )
    if produce_id:
        code, logs = req("GET", f"/api/steps/{produce_id}/logs", token=admin)
        if isinstance(logs, dict) and "logs" in logs:
            logs = logs["logs"]
        text = "\n".join(l.get("data", "") for l in logs) if isinstance(logs, list) else ""
        check("agent used presign path", "via presign" in text, text[-500:])
    else:
        check("agent used presign path", False, "no produce step")

    # Download: API 307 → MinIO; do not forward Authorization (MinIO returns 400).
    if isinstance(arts, list) and arts:
        aid = next(
            (a["id"] for a in arts if "VERSION" in a.get("name", "")),
            arts[0]["id"],
        )
        try:
            r = urllib.request.Request(
                f"{API}/api/artifacts/{aid}/download",
                headers={"Authorization": f"Bearer {admin}"},
                method="GET",
            )
            opener = urllib.request.build_opener(NoAuthRedirectHandler)
            with opener.open(r, timeout=30) as resp:
                body = resp.read()
                check("download non-empty", len(body) > 0, len(body))
                check("download VERSION content", b"s3-marker" in body, body[:80])
                final_url = resp.geturl()
                check(
                    "download redirected to MinIO",
                    "19000" in final_url or "fiber-artifacts" in final_url,
                    final_url,
                )
        except Exception as e:
            check("download", False, e)

    try:
        proc.terminate()
        proc.wait(timeout=5)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
    req("DELETE", f"/api/agents/{agent_id}", token=admin)

    print("---")
    if FAILS:
        print(f"DOGFOOD_FAIL failures={FAILS}")
        return 1
    print("DOGFOOD_OK s3-presign")
    return 0


if __name__ == "__main__":
    sys.exit(main())
