import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SELECTOR = Path(__file__).resolve().parent / "ci-scope.sh"


class CIScopeTests(unittest.TestCase):
    def test_branch_changes_select_actual_inputs_and_missing_history_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*arguments):
                return subprocess.check_output(
                    ["git", "-c", "user.name=CI test", "-c", "user.email=ci@example.invalid", *arguments],
                    cwd=root, text=True, stderr=subprocess.DEVNULL,
                ).strip()

            def commit():
                git("add", "--all")
                return git("commit", "-qm", "test change") or git("rev-parse", "HEAD")

            git("init", "-q", "--initial-branch=main")
            (root / "README.md").write_text("Documentation\n")
            (root / "Dockerfile").write_text("FROM scratch\n")
            commit()
            git("checkout", "-qb", "feature")
            (root / "README.md").write_text("Changed documentation\n")
            commit()
            git("checkout", "-q", "main")
            (root / "Dockerfile").write_text("FROM scratch\nLABEL upstream=changed\n")
            base = commit()
            git("checkout", "-q", "feature")

            def select(base_sha):
                output = root / "scope.out"
                output.unlink(missing_ok=True)
                result = subprocess.run(
                    ["bash", str(SELECTOR)], cwd=root, capture_output=True, text=True,
                    env=os.environ | {"GITHUB_EVENT_NAME": "pull_request", "BASE_SHA": base_sha,
                                      "GITHUB_OUTPUT": str(output)}, check=False,
                )
                flags = dict(line.split("=", 1) for line in output.read_text().splitlines()) if output.exists() else {}
                return result, flags

            result, flags = select(base)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(flags["rust"], "false")
            self.assertEqual(flags["image"], "false")
            (root / "Dockerfile").write_text("FROM scratch\nLABEL candidate=changed\n")
            commit()
            result, flags = select(base)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(flags["image"], "true")
            self.assertEqual(flags["rust"], "false")
            result, flags = select("0" * 40)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(flags, {})
