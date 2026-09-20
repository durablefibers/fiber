#!/usr/bin/env python3
"""Smoke: release-with-artifacts + path-filter webhook."""
from __future__ import annotations

import hashlib
import hmac
import json
import os
import shutil
import time
import urllib.error
import urllib.request

# Honour FIBER_API_URL. Hardcoding this made the script silently test whatever was on
# 18080 — which, when you are running a second API on another port to check a change,
# is the old build, and the smoke passes without having tested anything you wrote.
API = os.environ.get("FIBER_API_URL", "http://127.0.0.1:18080").rstrip("/")
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
WS_DIR = os.path.join(ROOT, "data", "workspaces")


def req(method: str, path: str, token: str | None = None, body: dict | None = None, headers: dict | None = None):
    data = None if body is None else json.dumps(body).encode()
    h = {"Content-Type": "application/json"}
    if token:
        h["Authorization"] = f"Bearer {token}"
    if headers:
        h.update(headers)
    r = urllib.request.Request(API + path, data=data, headers=h, method=method)
    with urllib.request.urlopen(r, timeout=30) as resp:
        raw = resp.read().decode()
        return json.loads(raw) if raw else {}


def main() -> None:
    ready = req("GET", "/ready")
    assert ready.get("ok"), ready
    print("ready", ready)

    login = req("POST", "/api/auth/login", body={"username": "admin", "password": "fiber"})
    token = login["token"]

    # This smoke drives a real pipeline, so it needs an agent already online — it does
    # not start one. Without this check the failure is a 90-second wait ending in
    # "run failed: running", which says nothing about the cause.
    agents = req("GET", "/api/agents", token=token)
    online = [a for a in agents if a.get("online")]
    assert online, (
        "no agent is online; start one (see docs/development.md) before smoke-artifacts. "
        "Note `make smoke-pools` terminates every agent on the host."
    )
    print("agents online", [a.get("name") for a in online])

    projects = req("GET", "/api/projects", token=token)
    project = next(p for p in projects if p.get("slug") == "showcase" or "showcase" in p.get("name", "").lower())
    pid = project["id"]
    print("project", pid, project.get("name"))

    pipes = req("GET", f"/api/projects/{pid}/pipelines", token=token)
    pipe = next(p for p in pipes if "release" in p["name"].lower())
    print("pipeline", pipe["id"], pipe["name"])

    run = req("POST", f"/api/pipelines/{pipe['id']}/runs", token=token, body={"trigger": "smoke:artifacts"})
    rid = run["run"]["id"]
    print("run", rid)

    wiped = False
    status = "pending"
    for i in range(90):
        time.sleep(1.5)
        run = req("GET", f"/api/runs/{rid}", token=token)
        status = run["run"]["status"]
        steps = run.get("steps") or req("GET", f"/api/runs/{rid}/steps", token=token)
        summary = " ".join(f"{s['step_id']}={s['status']}" for s in steps)
        print(f"t={i} run={status} {summary}")
        # Wipe the producer's own workspace, not the whole run's.
        #
        # This used to `rm -rf data/workspaces/<run_id>`, which also takes `.repo` —
        # the reference clone every step of the run fetches from, and the working
        # directory of whichever step is preparing right then. A dependent step is
        # routinely already `running` by the time this poll observes `build` as
        # succeeded, and it died with `fatal: unable to get current working directory`.
        # It passed only by winning a race: the wipe had to land in the gap between the
        # producer finishing and the consumer's `git fetch` starting. Anything that
        # shortens that gap (faster log ingest, for one) makes the smoke fail on a
        # product that is working correctly.
        #
        # A terminal step's directory has nothing running in it, so removing it cannot
        # race anything, and it still proves the point: the artifacts `build` wrote are
        # gone from the disk, so a later step that has them restored them.
        build = next(
            (s for s in steps if s["step_id"] == "build" and s["status"] == "succeeded"),
            None,
        )
        if build and not wiped:
            w = os.path.join(WS_DIR, rid, build["id"])
            if os.path.isdir(w):
                # The agent's own workspace GC releases this tree as the last step of
                # the run leaves it, so a directory can vanish under the walk. That is
                # the outcome this wants, not an error — but if anything is still there
                # afterwards, remove it again and let a real failure raise.
                shutil.rmtree(w, ignore_errors=True)
                if os.path.isdir(w):
                    shutil.rmtree(w)
            print("WIPED workspace", w)
            wiped = True
        if status in ("succeeded", "failed", "cancelled"):
            break

    arts = req("GET", f"/api/runs/{rid}/artifacts", token=token)
    print("artifacts", json.dumps(arts, indent=2))
    print("FINAL", status)

    # Path filters get a project of their own. Creating the pipeline in the seeded
    # showcase left one behind on every run — a well-used instance ended up firing 31
    # pipelines per webhook — and set a webhook secret on showcase that nothing removed.
    # A dedicated project also makes the assertions exact instead of "is ours in the set".
    paths_project = req(
        "POST",
        "/api/projects",
        token=token,
        body={"name": "Paths Smoke", "slug": f"paths-smoke-{int(time.time())}"},
    )
    paths_pid = paths_project["id"]
    print("paths project", paths_pid)

    created = req(
        "POST",
        f"/api/projects/{paths_pid}/pipelines",
        token=token,
        body={
            "name": "paths-test",
            "definition": {
                "name": "paths-test",
                "on": {
                    "push": {
                        "branches": ["main"],
                        "paths": ["src/**"],
                        "paths_ignore": ["**/*.md"],
                    }
                },
                "steps": [
                    {
                        "id": "t",
                        "name": "t",
                        "needs": [],
                        "run": "echo ok",
                        "labels": ["os=linux"],
                        "retries": 0,
                        "artifacts": [],
                    }
                ],
            },
        },
    )
    print("created paths-test", created.get("id"))
    paths_id = created["id"]

    md_only = {
        "ref": "refs/heads/main",
        "commits": [{"added": [], "modified": ["README.md"], "removed": []}],
        "head_commit": {"added": [], "modified": ["README.md"], "removed": []},
    }
    rust = {
        "ref": "refs/heads/main",
        "commits": [{"added": ["src/main.rs"], "modified": [], "removed": []}],
        "head_commit": {"added": ["src/main.rs"], "modified": [], "removed": []},
    }
    # Webhooks fail closed: unsigned / unconfigured deliveries are rejected.
    def webhook(payload, secret=None):
        raw = json.dumps(payload).encode()
        h = {"X-GitHub-Event": "push"}
        if secret is not None:
            h["X-Hub-Signature-256"] = "sha256=" + hmac.new(secret.encode(), raw, hashlib.sha256).hexdigest()
        r = urllib.request.Request(
            API + f"/api/projects/{paths_pid}/webhooks/github", data=raw, headers={"Content-Type": "application/json", **h}, method="POST"
        )
        try:
            with urllib.request.urlopen(r, timeout=30) as resp:
                return resp.status, json.loads(resp.read().decode() or "{}")
        except urllib.error.HTTPError as e:
            return e.code, {}

    code, _ = webhook(rust)
    assert code == 401, f"unsigned webhook should be rejected before a secret is set, got {code}"
    wh_secret = "smoke-webhook-secret"
    req("PUT", f"/api/projects/{paths_pid}/webhooks/github", token=token, body={"secret": wh_secret})
    code, _ = webhook(rust)
    assert code == 401, f"unsigned webhook should be rejected once a secret is set, got {code}"
    code, _ = webhook(rust, secret="wrong-secret")
    assert code == 401, f"badly signed webhook should be rejected, got {code}"
    print("webhook unsigned/badly-signed rejected (401)")
    c1, r1 = webhook(md_only, secret=wh_secret)
    c2, r2 = webhook(rust, secret=wh_secret)
    assert c1 == 200 and c2 == 200, (c1, c2)
    print("webhook md_only", r1)
    print("webhook rust", r2)

    # Resolve which started runs belong to paths-test
    def pipeline_ids_for_runs(run_ids):
        out = []
        for rid_ in run_ids:
            detail = req("GET", f"/api/runs/{rid_}", token=token)
            out.append(detail["run"]["pipeline_id"])
        return out

    md_pipes = set(pipeline_ids_for_runs(r1.get("started", [])))
    rust_pipes = set(pipeline_ids_for_runs(r2.get("started", [])))
    print("md pipelines", md_pipes)
    print("rust pipelines", rust_pipes)
    # The project holds exactly one pipeline, so these are equalities rather than
    # "ours is somewhere in the set".
    assert md_pipes == set(), f"README-only push should start nothing, started {md_pipes}"
    assert rust_pipes == {paths_id}, f"src/** push should start only paths-test, started {rust_pipes}"
    assert status == "succeeded", f"run failed: {status}"
    names = {a.get("name") for a in arts}
    assert names & {"out/VERSION", "out/release.tar", "out/release.tar.sig"} or any(
        "VERSION" in n or "release" in n for n in names
    ), arts

    # Confirm restore / upload showed up in step system logs
    restore_hits = 0
    upload_hits = 0
    for s in steps:
        logs = req("GET", f"/api/steps/{s['id']}/logs", token=token)
        for line in logs:
            d = line.get("data", "")
            if "restoring artifact" in d:
                restore_hits += 1
            if "uploading artifact" in d:
                upload_hits += 1
    print(f"log_hits restore={restore_hits} upload={upload_hits}")
    assert upload_hits >= 2, "expected HTTP uploads from build/sign"
    assert wiped, "the producer's workspace was never wiped; the restore proves nothing"
    assert restore_hits >= 1, "expected at least one restore after workspace wipe"

    # Only on a pass: a failed smoke keeps its project, because the pipelines, runs and
    # logs inside it are the only record of what went wrong. Every assert above raises,
    # so reaching here means the run was green.
    deleted = req("DELETE", f"/api/projects/{paths_pid}", token=token)
    print("cleaned up paths project", paths_pid, deleted)
    print("SMOKE_OK")


if __name__ == "__main__":
    main()
