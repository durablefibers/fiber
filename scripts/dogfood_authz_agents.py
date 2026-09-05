#!/usr/bin/env python3
"""Smoke: authz roles, members, agent CRUD/rotate."""
from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request

API = "http://127.0.0.1:18080"
FAILS = 0


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


def main() -> int:
    code, ready = req("GET", "/ready")
    check("ready", code == 200 and ready.get("ok") is True, ready)

    code, login = req("POST", "/api/auth/login", body={"username": "admin", "password": "fiber"})
    check("login", code == 200 and "token" in login, login)
    admin = login["token"]

    code, projects = req("GET", "/api/projects", token=admin)
    check("list projects", code == 200 and isinstance(projects, list) and len(projects) > 0, projects)
    showcase = next((p for p in projects if p.get("slug") == "showcase"), projects[0])
    pid = showcase["id"]

    code, proj = req("GET", f"/api/projects/{pid}", token=admin)
    check("get project role", code == 200 and proj.get("role") in ("owner", "admin"), proj)

    # Create reader user via add member
    code, add = req(
        "POST",
        f"/api/projects/{pid}/members",
        token=admin,
        body={"username": "dogfood_reader", "role": "reader", "password": "fiber-reader"},
    )
    check("add reader member", code == 200 and add.get("ok"), add)

    code, members = req("GET", f"/api/projects/{pid}/members", token=admin)
    check(
        "list members",
        code == 200 and any(m.get("username") == "dogfood_reader" for m in members),
        members,
    )

    code, rlogin = req(
        "POST", "/api/auth/login", body={"username": "dogfood_reader", "password": "fiber-reader"}
    )
    check("reader login", code == 200 and "token" in rlogin, rlogin)
    reader = rlogin["token"]

    code, _ = req("GET", f"/api/projects/{pid}/pipelines", token=reader)
    check("reader can list pipelines", code == 200)

    code, denied = req(
        "POST",
        f"/api/projects/{pid}/pipelines",
        token=reader,
        body={
            "name": "should-fail",
            "definition": {
                "name": "should-fail",
                "steps": [{"id": "t", "name": "t", "needs": [], "run": "echo", "labels": ["os=linux"]}],
            },
        },
    )
    check("reader cannot create pipeline", code == 403, denied)

    code, secrets_denied = req("GET", f"/api/projects/{pid}/secrets", token=reader)
    check("reader cannot list secrets", code == 403, secrets_denied)

    # Agent CRUD + rotate
    code, created = req(
        "POST",
        "/api/agents",
        token=admin,
        body={"name": "dogfood-agent", "labels": ["os=linux", "dogfood=true"], "concurrency": 2},
    )
    check("create agent", code == 201 and "token" in created and "agent" in created, created)
    aid = created["agent"]["id"]
    token1 = created["token"]

    code, updated = req(
        "PUT",
        f"/api/agents/{aid}",
        token=admin,
        body={"concurrency": 3, "labels": ["os=linux", "dogfood=rotated"]},
    )
    check(
        "update agent",
        code == 200 and updated.get("concurrency") == 3,
        updated,
    )

    code, rotated = req("POST", f"/api/agents/{aid}/rotate-token", token=admin)
    check("rotate token", code == 200 and "token" in rotated and rotated["token"] != token1, rotated)

    code, agents = req("GET", "/api/agents", token=admin)
    check(
        "list agents includes dogfood",
        code == 200 and any(a.get("id") == aid for a in agents),
        agents,
    )

    code, deleted = req("DELETE", f"/api/agents/{aid}", token=admin)
    check("delete agent", code == 200 and deleted.get("ok"), deleted)

    code, agents2 = req("GET", "/api/agents", token=admin)
    check(
        "agent gone after delete",
        code == 200 and not any(a.get("id") == aid for a in agents2),
        agents2,
    )

    # Cleanup reader membership (keep user for re-runs)
    reader_id = next(m["user_id"] for m in members if m.get("username") == "dogfood_reader")
    # refresh members after add may have different shape
    code, members2 = req("GET", f"/api/projects/{pid}/members", token=admin)
    reader_id = next(m["user_id"] for m in members2 if m.get("username") == "dogfood_reader")
    code, rm = req("DELETE", f"/api/projects/{pid}/members/{reader_id}", token=admin)
    check("remove reader member", code == 200 and rm.get("ok"), rm)

    print("---")
    if FAILS:
        print(f"DOGFOOD_FAIL failures={FAILS}")
        return 1
    print("DOGFOOD_OK authz+agents")
    return 0


if __name__ == "__main__":
    sys.exit(main())
