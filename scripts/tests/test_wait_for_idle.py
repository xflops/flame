"""Exercise benchmark readiness against paginated CLI JSON without a cluster."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[2] / "testing/system/perf/wait_for_idle.sh"


@unittest.skipUnless(shutil.which("jq"), "jq is required")
class WaitForIdleTest(unittest.TestCase):
    def run_wait(self, sessions, executors):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "pages.json").write_text(json.dumps({"-s": sessions, "-e": executors}))
            cli = root / "flmctl"
            cli.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "number = int(args[args.index('--number') + 1]) if '--number' in args else 1\n"
                "with open(os.environ['PAGES']) as source: pages = json.load(source)\n"
                "print(json.dumps(pages[args[1]][number - 1]))\n"
            )
            cli.chmod(0o755)
            return subprocess.run(
                ["bash", str(SCRIPT), "0"],
                env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}", "PAGES": str(root / "pages.json")},
                capture_output=True,
                text=True,
                timeout=10,
            )

    @staticmethod
    def page(states, number=1, total=None):
        return {
            "items": [{"state": state} for state in states],
            "paging": {"limit": 1, "number": number, "total": len(states) if total is None else total},
        }

    def test_empty_envelopes_are_idle(self):
        result = self.run_wait([self.page([])], [self.page([])])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cluster is idle", result.stdout)

    def test_closed_sessions_and_released_executors_are_idle(self):
        result = self.run_wait([self.page(["Closed"])], [self.page(["Released"])])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_open_session_on_later_page_prevents_idle(self):
        result = self.run_wait(
            [self.page(["Closed"], total=2), self.page(["Open"], number=2, total=2)],
            [self.page([])],
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("1 open sessions", result.stdout)

    def test_unreleased_executor_on_later_page_prevents_idle(self):
        result = self.run_wait(
            [self.page([])],
            [self.page(["Released"], total=2), self.page(["Idle"], number=2, total=2)],
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("1 unreleased executors", result.stdout)

    def test_missing_total_cannot_report_idle(self):
        page = self.page([])
        del page["paging"]["total"]
        result = self.run_wait([page], [self.page([])])
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("cluster is idle", result.stdout)
