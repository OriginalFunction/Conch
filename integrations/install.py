#!/usr/bin/env python3
"""Install the Conch skill for one agent host without invoking that host."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import tempfile


REPO_ROOT = Path(__file__).resolve().parents[1]
INTEGRATIONS_ROOT = REPO_ROOT / "integrations"
CANONICAL_SKILL = REPO_ROOT / "skills" / "join-room" / "SKILL.md"
CODEX_PLUGIN = INTEGRATIONS_ROOT / "codex" / "plugins" / "conch"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Install Conch agent packaging and print the MCP setup."
    )
    parser.add_argument("host", choices=("codex", "claude", "generic"))
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.home(),
        help="Home-like installation root (default: current user's home)",
    )
    parser.add_argument(
        "--skill-root",
        type=Path,
        help="Generic skill directory (default: ROOT/.agents/skills)",
    )
    parser.add_argument(
        "--agent",
        help="Stable Conch agent id (defaults by host)",
    )
    parser.add_argument(
        "--conch-command",
        default="conch",
        help="Installed conch executable or absolute path",
    )
    return parser.parse_args()


def copy_if_changed(source: Path, destination: Path) -> None:
    contents = source.read_bytes()
    if destination.is_file() and destination.read_bytes() == contents:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as temporary:
        temporary.write(contents)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    os.replace(temporary_path, destination)


def write_json_if_changed(destination: Path, payload: object) -> None:
    encoded = (json.dumps(payload, indent=2) + "\n").encode()
    if destination.is_file() and destination.read_bytes() == encoded:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as temporary:
        temporary.write(encoded)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    os.replace(temporary_path, destination)


def mcp_config(command: str, agent: str) -> dict[str, object]:
    return {
        "mcpServers": {
            "conch": {
                "command": command,
                "args": ["--agent", agent, "mcp"],
            }
        }
    }


def install_codex(root: Path, command: str, agent: str) -> dict[str, object]:
    plugin_destination = root / "plugins" / "conch"
    for relative in (
        Path(".codex-plugin/plugin.json"),
        Path(".mcp.json"),
        Path("skills/join-room/SKILL.md"),
    ):
        copy_if_changed(CODEX_PLUGIN / relative, plugin_destination / relative)

    installed_mcp = plugin_destination / ".mcp.json"
    write_json_if_changed(installed_mcp, mcp_config(command, agent))

    marketplace_path = root / ".agents" / "plugins" / "marketplace.json"
    if marketplace_path.exists():
        marketplace = json.loads(marketplace_path.read_text(encoding="utf-8"))
        if not isinstance(marketplace, dict):
            raise ValueError(f"{marketplace_path} must contain an object")
        marketplace_name = marketplace.get("name")
        plugins = marketplace.get("plugins")
        if not isinstance(marketplace_name, str) or not marketplace_name:
            raise ValueError(f"{marketplace_path} requires a non-empty name")
        if not isinstance(plugins, list):
            raise ValueError(f"{marketplace_path} requires a plugins array")
    else:
        marketplace_name = "personal"
        marketplace = {
            "name": marketplace_name,
            "interface": {"displayName": "Personal"},
            "plugins": [],
        }
        plugins = marketplace["plugins"]

    entry = {
        "name": "conch",
        "source": {"source": "local", "path": "./plugins/conch"},
        "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
        "category": "Productivity",
    }
    for index, plugin in enumerate(plugins):
        if isinstance(plugin, dict) and plugin.get("name") == "conch":
            plugins[index] = entry
            break
    else:
        plugins.append(entry)
    write_json_if_changed(marketplace_path, marketplace)
    return {
        "host": "codex",
        "skill": str(plugin_destination / "skills" / "join-room" / "SKILL.md"),
        "plugin": str(plugin_destination),
        "marketplace": str(marketplace_path),
        "next_command": ["codex", "plugin", "add", f"conch@{marketplace_name}"],
    }


def install_skill(
    host: str, skill_root: Path, command: str, agent: str
) -> dict[str, object]:
    skill_destination = skill_root / "join-room" / "SKILL.md"
    copy_if_changed(CANONICAL_SKILL, skill_destination)
    config = mcp_config(command, agent)
    result: dict[str, object] = {
        "host": host,
        "skill": str(skill_destination),
        "mcp_config": config,
    }
    if host == "claude":
        result["next_command"] = [
            "claude",
            "mcp",
            "add",
            "--scope",
            "user",
            "conch",
            "--",
            command,
            "--agent",
            agent,
            "mcp",
        ]
    else:
        result["grok_command"] = [
            "grok",
            "mcp",
            "add",
            "--scope",
            "user",
            "conch",
            "--",
            command,
            "--agent",
            agent,
            "mcp",
        ]
    return result


def main() -> None:
    args = parse_args()
    root = args.root.expanduser().resolve()
    defaults = {
        "codex": "agent:codex",
        "claude": "agent:claude",
        "generic": "agent:generic",
    }
    agent = args.agent or defaults[args.host]
    if re.fullmatch(r"[a-z0-9_.:-]{1,64}", agent) is None:
        raise ValueError("--agent must match [a-z0-9_.:-]+ and be at most 64 characters")

    if args.host == "codex":
        result = install_codex(root, args.conch_command, agent)
    else:
        default_skill_root = (
            root / ".claude" / "skills"
            if args.host == "claude"
            else root / ".agents" / "skills"
        )
        skill_root = (args.skill_root or default_skill_root).expanduser().resolve()
        result = install_skill(args.host, skill_root, args.conch_command, agent)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
