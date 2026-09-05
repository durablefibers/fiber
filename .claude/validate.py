#!/usr/bin/env python3
"""Validate the Fiber Claude Code toolkit.

Skills are checked against the Agent Skills specification (agentskills.io/specification)
so they stay portable to any skills-compatible agent, not just Claude Code.
Subagents are checked against the Claude Code subagent frontmatter rules.
Hooks are checked for existence, executability, and registration in settings.json.

Run: python3 .claude/validate.py
"""
from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent
errors: list[str] = []
warnings: list[str] = []
checked = {"skills": 0, "agents": 0, "hooks": 0}

# Fields the Agent Skills spec allows in SKILL.md frontmatter. Claude Code accepts
# more, but staying inside this set keeps every skill portable and packageable.
SPEC_FIELDS = {"name", "description", "license", "compatibility", "metadata", "allowed-tools"}
NAME_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")

# Claude Code subagent frontmatter.
AGENT_REQUIRED = {"name", "description"}
AGENT_KNOWN = AGENT_REQUIRED | {
    "tools", "disallowedTools", "model", "permissionMode", "maxTurns", "skills",
    "mcpServers", "hooks", "memory", "background", "effort", "isolation", "color",
    "initialPrompt", "experimental",
}
AGENT_MODELS = {"sonnet", "opus", "haiku", "fable", "inherit"}
AGENT_COLORS = {"red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan"}


def split_frontmatter(path: Path) -> dict | None:
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---\n"):
        errors.append(f"{path}: frontmatter must start on line 1 with ---")
        return None
    end = text.find("\n---", 3)
    if end == -1:
        errors.append(f"{path}: unterminated frontmatter")
        return None
    try:
        data = yaml.safe_load(text[4:end])
    except yaml.YAMLError as exc:
        errors.append(f"{path}: invalid YAML: {exc}")
        return None
    if not isinstance(data, dict):
        errors.append(f"{path}: frontmatter is not a mapping")
        return None
    return data


def check_skill(skill_md: Path) -> None:
    checked["skills"] += 1
    rel = skill_md.relative_to(ROOT.parent)
    fm = split_frontmatter(skill_md)
    if fm is None:
        return

    extra = set(fm) - SPEC_FIELDS
    if extra:
        errors.append(f"{rel}: fields outside the Agent Skills spec: {sorted(extra)}")

    name = fm.get("name")
    if not name:
        errors.append(f"{rel}: missing required 'name'")
    else:
        if len(name) > 64:
            errors.append(f"{rel}: name exceeds 64 chars ({len(name)})")
        if not NAME_RE.match(str(name)):
            errors.append(f"{rel}: name must be lowercase alphanumeric with single hyphens, "
                          f"no leading/trailing/consecutive hyphens: {name!r}")
        if name != skill_md.parent.name:
            errors.append(f"{rel}: name {name!r} must match directory {skill_md.parent.name!r}")

    desc = fm.get("description")
    if not desc:
        errors.append(f"{rel}: missing required 'description'")
    else:
        if len(desc) > 1024:
            errors.append(f"{rel}: description exceeds 1024 chars ({len(desc)})")
        if len(desc) < 40:
            warnings.append(f"{rel}: description is thin ({len(desc)} chars); "
                            "say what it does AND when to use it")
        if " use " not in desc.lower():
            warnings.append(f"{rel}: description has no 'Use when...' trigger clause")

    compat = fm.get("compatibility")
    if compat and len(compat) > 500:
        errors.append(f"{rel}: compatibility exceeds 500 chars ({len(compat)})")

    meta = fm.get("metadata")
    if meta is not None:
        if not isinstance(meta, dict) or not all(
            isinstance(k, str) and isinstance(v, str) for k, v in meta.items()
        ):
            errors.append(f"{rel}: metadata must be a map of string keys to string values "
                          "(quote version numbers)")

    body_lines = skill_md.read_text(encoding="utf-8").splitlines()
    if len(body_lines) > 500:
        warnings.append(f"{rel}: {len(body_lines)} lines; spec recommends under 500 — "
                        "move detail into references/")

    # Referenced files must exist (progressive disclosure that dead-ends is a bug).
    for link in re.findall(r"\[[^\]]+\]\(([^)]+)\)", skill_md.read_text(encoding="utf-8")):
        if link.startswith(("http://", "https://", "#")):
            continue
        if not (skill_md.parent / link).exists():
            errors.append(f"{rel}: references missing file {link}")


def check_agent(path: Path) -> None:
    checked["agents"] += 1
    rel = path.relative_to(ROOT.parent)
    fm = split_frontmatter(path)
    if fm is None:
        return

    missing = AGENT_REQUIRED - set(fm)
    if missing:
        errors.append(f"{rel}: missing required field(s): {sorted(missing)}")

    unknown = set(fm) - AGENT_KNOWN
    if unknown:
        warnings.append(f"{rel}: unrecognized frontmatter field(s): {sorted(unknown)}")

    name = fm.get("name", "")
    if name:
        if ":" in name or name.startswith("-"):
            errors.append(f"{rel}: name must not contain ':' or start with '-': {name!r}")
        if name != path.stem:
            warnings.append(f"{rel}: name {name!r} differs from filename {path.stem!r}")

    model = fm.get("model")
    if model and model not in AGENT_MODELS and not str(model).startswith("claude-"):
        warnings.append(f"{rel}: unusual model {model!r} "
                        f"(expected one of {sorted(AGENT_MODELS)} or a full model id)")

    color = fm.get("color")
    if color and color not in AGENT_COLORS:
        errors.append(f"{rel}: color {color!r} not in {sorted(AGENT_COLORS)}")

    tools = fm.get("tools")
    if isinstance(tools, str):
        for tool in [t.strip() for t in tools.split(",") if t.strip()]:
            if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*(\(.*\))?$", tool):
                warnings.append(f"{rel}: suspicious tool entry {tool!r}")
    elif tools is not None:
        errors.append(f"{rel}: 'tools' must be a comma-separated string, not {type(tools).__name__}")

    desc = fm.get("description", "")
    if desc and len(desc) < 40:
        warnings.append(f"{rel}: description is thin; it is what routes delegation")


def check_hooks() -> None:
    settings_path = ROOT / "settings.json"
    if not settings_path.exists():
        errors.append(".claude/settings.json is missing")
        return
    try:
        settings = json.loads(settings_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        errors.append(f".claude/settings.json: invalid JSON: {exc}")
        return

    registered: set[str] = set()
    for event, entries in settings.get("hooks", {}).items():
        if not isinstance(entries, list):
            errors.append(f"settings.json: hooks.{event} must be a list")
            continue
        for entry in entries:
            for handler in entry.get("hooks", []):
                checked["hooks"] += 1
                if handler.get("type") != "command":
                    warnings.append(f"settings.json: hooks.{event} handler type "
                                    f"{handler.get('type')!r} is not 'command'")
                cmd = handler.get("command", "")
                match = re.search(r"\.claude/hooks/([\w.-]+)", cmd)
                if not match:
                    continue
                script = ROOT / "hooks" / match.group(1)
                registered.add(match.group(1))
                if not script.exists():
                    errors.append(f"settings.json: hooks.{event} points at missing {script.name}")
                elif not os.access(script, os.X_OK):
                    errors.append(f"{script.name} is registered but not executable (chmod +x)")

    for script in sorted((ROOT / "hooks").glob("*.sh")):
        if script.name not in registered and script.name != "selftest.sh":
            warnings.append(f"hooks/{script.name} exists but is not registered in settings.json")


def main() -> int:
    for skill_md in sorted((ROOT / "skills").glob("*/SKILL.md")):
        check_skill(skill_md)
    for agent in sorted((ROOT / "agents").glob("*.md")):
        check_agent(agent)
    check_hooks()

    for w in warnings:
        print(f"  warn  {w}")
    for e in errors:
        print(f"  ERROR {e}")

    print(f"\n{checked['skills']} skills, {checked['agents']} agents, "
          f"{checked['hooks']} hook handlers checked — "
          f"{len(errors)} error(s), {len(warnings)} warning(s)")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
