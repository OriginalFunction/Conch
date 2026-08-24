from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
INSTALLER = REPO_ROOT / "integrations" / "install.py"
VALIDATOR = REPO_ROOT / "integrations" / "validate.py"
CANONICAL_SKILL = REPO_ROOT / "skills" / "join-room" / "SKILL.md"


def run_installer(*arguments: str) -> dict[str, object]:
    completed = subprocess.run(
        [sys.executable, str(INSTALLER), *arguments],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


class InstallerTests(unittest.TestCase):
    def test_codex_fresh_root_creates_personal_marketplace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = run_installer("codex", "--root", str(root))
            self.assertEqual(
                result["next_command"],
                ["codex", "plugin", "add", "conch@personal"],
            )
            marketplace = json.loads(
                (root / ".agents" / "plugins" / "marketplace.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(marketplace["name"], "personal")
            self.assertEqual(marketplace["plugins"][0]["name"], "conch")

    def test_codex_install_is_idempotent_and_preserves_marketplace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marketplace = root / ".agents" / "plugins" / "marketplace.json"
            marketplace.parent.mkdir(parents=True)
            marketplace.write_text(
                json.dumps(
                    {
                        "name": "team-local",
                        "interface": {"displayName": "Team"},
                        "plugins": [{"name": "existing"}],
                    }
                ),
                encoding="utf-8",
            )
            arguments = (
                "codex",
                "--root",
                str(root),
                "--agent",
                "agent:test-codex",
                "--conch-command",
                "/opt/conch/bin/conch",
            )
            first = run_installer(*arguments)
            first_bytes = marketplace.read_bytes()
            second = run_installer(*arguments)
            self.assertEqual(first, second)
            self.assertEqual(first_bytes, marketplace.read_bytes())
            self.assertEqual(
                first["next_command"],
                ["codex", "plugin", "add", "conch@team-local"],
            )

            installed = json.loads(marketplace.read_text(encoding="utf-8"))
            self.assertEqual(
                [entry["name"] for entry in installed["plugins"]],
                ["existing", "conch"],
            )
            plugin = root / "plugins" / "conch"
            self.assertEqual(
                (plugin / "skills" / "join-room" / "SKILL.md").read_bytes(),
                CANONICAL_SKILL.read_bytes(),
            )
            mcp = json.loads((plugin / ".mcp.json").read_text(encoding="utf-8"))
            self.assertEqual(
                mcp["mcpServers"]["conch"],
                {
                    "command": "/opt/conch/bin/conch",
                    "args": ["--agent", "agent:test-codex", "mcp"],
                },
            )

    def test_claude_install_is_idempotent_and_prints_exact_command(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            arguments = ("claude", "--root", str(root), "--agent", "agent:claude-test")
            first = run_installer(*arguments)
            second = run_installer(*arguments)
            self.assertEqual(first, second)
            self.assertEqual(
                first["next_command"],
                [
                    "claude",
                    "mcp",
                    "add",
                    "--scope",
                    "user",
                    "conch",
                    "--",
                    "conch",
                    "--agent",
                    "agent:claude-test",
                    "mcp",
                ],
            )
            installed_skill = root / ".claude" / "skills" / "join-room" / "SKILL.md"
            self.assertEqual(installed_skill.read_bytes(), CANONICAL_SKILL.read_bytes())

    def test_generic_install_uses_requested_skill_root_and_prints_mcp_config(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            skill_root = root / "host-skills"
            result = run_installer(
                "generic",
                "--root",
                str(root),
                "--skill-root",
                str(skill_root),
                "--agent",
                "agent:grok-test",
            )
            self.assertEqual(
                result["mcp_config"]["mcpServers"]["conch"],
                {
                    "command": "conch",
                    "args": ["--agent", "agent:grok-test", "mcp"],
                },
            )
            self.assertEqual(
                result["grok_command"],
                [
                    "grok",
                    "mcp",
                    "add",
                    "--scope",
                    "user",
                    "conch",
                    "--",
                    "conch",
                    "--agent",
                    "agent:grok-test",
                    "mcp",
                ],
            )
            self.assertEqual(
                (skill_root / "join-room" / "SKILL.md").read_bytes(),
                CANONICAL_SKILL.read_bytes(),
            )

    def test_checked_in_integrations_validate(self) -> None:
        subprocess.run(
            [sys.executable, str(VALIDATOR)],
            cwd=REPO_ROOT,
            check=True,
            capture_output=True,
            text=True,
        )

    def test_invalid_agent_identity_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            completed = subprocess.run(
                [
                    sys.executable,
                    str(INSTALLER),
                    "generic",
                    "--root",
                    temporary,
                    "--agent",
                    "Agent With Spaces",
                ],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("[a-z0-9_.:-]+", completed.stderr)


if __name__ == "__main__":
    unittest.main()
