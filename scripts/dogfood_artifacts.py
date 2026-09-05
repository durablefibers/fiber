#!/usr/bin/env python3
"""Dogfood release-with-artifacts + path-filter webhook smoke."""
from __future__ import annotations

import json
import os
import shutil
import time
import urllib.request

API = "http://127.0.0.1:18080"
WS_DIR = "/Users/blake/Projects/durablefibers/data/workspaces"


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

    projects = req("GET", "/api/projects", token=token)
    project = next(p for p in projects if p.get("slug") == "showcase" or "showcase" in p.get("name", "").lower())
    pid = project["id"]
    print("project", pid, project.get("name"))

    pipes = req("GET", f"/api/projects/{pid}/pipelines", token=token)
    pipe = next(p for p in pipes if "release" in p["name"].lower())
    print("pipeline", pipe["id"], pipe["name"])

    run = req("POST", f"/api/pipelines/{pipe['id']}/runs", token=token, body={"trigger": "dogfood:artifacts"})
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
        build_ok = any(s["step_id"] == "build" and s["status"] == "succeeded" for s in steps)
        if build_ok and not wiped:
            w = os.path.join(WS_DIR, rid)
            if os.path.isdir(w):
                shutil.rmtree(w)
                print("WIPED workspace", w)
            wiped = True
        if status in ("succeeded", "failed", "cancelled"):
            break

    arts = req("GET", f"/api/runs/{rid}/artifacts", token=token)
    print("artifacts", json.dumps(arts, indent=2))
    print("FINAL", status)

    # Path filter pipeline
    created = req(
        "POST",
        f"/api/projects/{pid}/pipelines",
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
    r1 = req("POST", f"/api/projects/{pid}/webhooks/github", body=md_only, headers={"X-GitHub-Event": "push"})
    r2 = req("POST", f"/api/projects/{pid}/webhooks/github", body=rust, headers={"X-GitHub-Event": "push"})
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
    assert paths_id not in md_pipes, "paths-test should NOT fire on README-only push"
    assert paths_id in rust_pipes, "paths-test should fire on src/** push"
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
    assert restore_hits >= 1, "expected at least one restore after workspace wipe"
    print("DOGFOOD_OK")


if __name__ == "__main__":
    main()
