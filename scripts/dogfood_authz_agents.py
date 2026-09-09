#!/usr/bin/env python3
"""Smoke: authz roles, members, agent CRUD/rotate."""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

# Honour FIBER_API_URL. Hardcoding this made the script silently test whatever was on
# 18080 — which, when you are running a second API on another port to check a change,
# is the old build, and the smoke passes without having tested anything you wrote.
API = os.environ.get("FIBER_API_URL", "http://127.0.0.1:18080").rstrip("/")
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

    # Instance-admin gating: global agents are not for ordinary members.
    code, ga = req(
        "POST",
        "/api/agents",
        token=reader,
        body={"name": "should-fail", "labels": ["os=linux"], "concurrency": 1},
    )
    check("reader cannot create global agent", code == 403, ga)
    code, pa = req(
        "POST",
        "/api/agents",
        token=reader,
        body={"name": "should-fail", "labels": ["os=linux"], "concurrency": 1, "project_id": pid},
    )
    check("reader cannot create project agent", code == 403, pa)
    code, la = req("GET", "/api/agents", token=reader)
    check("reader cannot list all agents", code == 403, la)
    code, lpa = req("GET", f"/api/agents?project_id={pid}", token=reader)
    check("reader can list project agents", code == 200 and isinstance(lpa, list), lpa)
    code, me = req("GET", "/api/auth/me", token=admin)
    check("bootstrap admin is instance admin", code == 200 and me.get("is_admin") is True, me)
    code, users_denied = req("GET", "/api/users", token=reader)
    check("reader cannot list users", code == 403, users_denied)
    code, cu_denied = req("POST", "/api/users", token=reader, body={"username": "x", "password": "y"})
    check("reader cannot create users", code == 403, cu_denied)

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

    code, rot_denied = req("POST", f"/api/agents/{aid}/rotate-token", token=reader)
    check("reader cannot rotate global agent", code == 403, rot_denied)
    code, upd_denied = req("PUT", f"/api/agents/{aid}", token=reader, body={"concurrency": 9})
    check("reader cannot update global agent", code == 403, upd_denied)
    code, del_denied = req("DELETE", f"/api/agents/{aid}", token=reader)
    check("reader cannot delete global agent", code == 403, del_denied)

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

    # --- Account: own password, and revoking sessions -------------------------------
    # A throwaway user, so a failure here cannot lock anyone out of the dev instance.
    pw_user, pw_first, pw_second = "dogfood_pw", "initial-pass-1", "changed-pass-2"
    req("POST", "/api/users", token=admin, body={"username": pw_user, "password": pw_first})

    def login(password: str):
        code, body = req("POST", "/api/auth/login", body={"username": pw_user, "password": password})
        return code, (body or {}).get("token")

    code, t1 = login(pw_first)
    if code != 200:
        # A re-run: the password was changed last time round.
        code, t1 = login(pw_second)
        pw_first, pw_second = pw_second, pw_first
    check("login as the password test user", code == 200 and bool(t1), code)
    _, t2 = login(pw_first)

    code, _ = req("POST", "/api/auth/password", token=t1,
                  body={"current_password": "wrong", "new_password": "some-long-password"})
    check("wrong current password is refused", code == 401, code)

    code, _ = req("POST", "/api/auth/password", token=t1,
                  body={"current_password": pw_first, "new_password": "short"})
    check("too-short new password is refused", code == 400, code)

    code, _ = req("POST", "/api/auth/password", token=t1,
                  body={"current_password": pw_first, "new_password": pw_second})
    check("password change succeeds", code == 200, code)

    code, _ = req("GET", "/api/auth/me", token=t1)
    check("the session that changed it survives", code == 200, code)
    code, _ = req("GET", "/api/auth/me", token=t2)
    check("other sessions are dropped by a password change", code == 401, code)
    code, _ = login(pw_first)
    check("the old password no longer works", code == 401, code)
    code, t3 = login(pw_second)
    check("the new password works", code == 200 and bool(t3), code)

    _, t4 = login(pw_second)
    code, revoked = req("DELETE", "/api/auth/sessions", token=t3)
    check("revoke reports what it removed", code == 200 and revoked.get("revoked", 0) >= 1, revoked)
    code, _ = req("GET", "/api/auth/me", token=t3)
    check("the caller's session survives a revoke", code == 200, code)
    code, _ = req("GET", "/api/auth/me", token=t4)
    check("other sessions are revoked", code == 401, code)
    code, _ = req("GET", "/api/auth/me", token=admin)
    check("another user's session is untouched", code == 200, code)
    code, _ = req("DELETE", "/api/auth/sessions")
    check("revoke needs authentication", code == 401, code)

    print("---")
    if FAILS:
        print(f"DOGFOOD_FAIL failures={FAILS}")
        return 1
    print("DOGFOOD_OK authz+agents")
    return 0


if __name__ == "__main__":
    sys.exit(main())
