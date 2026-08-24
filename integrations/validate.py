#!/usr/bin/env python3
"""Deterministically validate Conch's checked-in agent integrations."""

from __future__ import annotations

import json
from pathlib import Path
import re


REPO_ROOT = Path(__file__).resolve().parents[1]
INTEGRATIONS_ROOT = REPO_ROOT / "integrations"
CANONICAL_SKILL = REPO_ROOT / "skills" / "join-room" / "SKILL.md"
PLUGIN_ROOT = INTEGRATIONS_ROOT / "codex" / "plugins" / "conch"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def load_json(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text(encoding="utf-8"))
    require(isinstance(value, dict), f"{path} must contain an object")
    return value


def main() -> None:
    skill = CANONICAL_SKILL.read_text(encoding="utf-8")
    require(skill.startswith("---\n"), "skill must start with YAML frontmatter")
    require("\n---\n" in skill[4:], "skill frontmatter must be closed")
    frontmatter = skill.split("---", 2)[1]
    require(re.search(r"(?m)^name: join-room$", frontmatter) is not None, "skill name")
    require(
        re.search(r"(?m)^description: \S", frontmatter) is not None,
        "skill description",
    )

    plugin_skill = PLUGIN_ROOT / "skills" / "join-room" / "SKILL.md"
    require(
        plugin_skill.read_bytes() == CANONICAL_SKILL.read_bytes(),
        "Codex plugin skill differs from canonical skills/join-room/SKILL.md",
    )

    manifest = load_json(PLUGIN_ROOT / ".codex-plugin" / "plugin.json")
    require(manifest.get("name") == PLUGIN_ROOT.name, "plugin name must match directory")
    require(manifest.get("skills") == "./skills/", "plugin must discover ./skills/")
    require(manifest.get("mcpServers") == "./.mcp.json", "plugin MCP path")

    mcp = load_json(PLUGIN_ROOT / ".mcp.json")
    server = mcp.get("mcpServers", {}).get("conch", {})  # type: ignore[union-attr]
    require(server.get("command") == "conch", "plugin must launch installed conch")
    require(
        server.get("args") == ["--agent", "agent:codex", "mcp"],
        "plugin must set an explicit Codex identity",
    )

    marketplace = load_json(
        INTEGRATIONS_ROOT / "codex" / ".agents" / "plugins" / "marketplace.json"
    )
    entries = marketplace.get("plugins")
    require(isinstance(entries, list), "marketplace plugins must be an array")
    matching = [entry for entry in entries if entry.get("name") == "conch"]
    require(len(matching) == 1, "marketplace must contain exactly one Conch entry")
    require(
        matching[0].get("source")
        == {"source": "local", "path": "./plugins/conch"},
        "marketplace source path",
    )
    print("agent integration validation passed")


if __name__ == "__main__":
    main()
