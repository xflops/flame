"""Verify shared publication decisions without contacting GitHub or publishing data."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("publication.sh")


class PublicationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "gh.json"
        gh = self.root / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "Path(os.environ['GH_LOG']).write_text(json.dumps(sys.argv[1:]))\n"
            "print(os.environ.get('GH_RESULT', ''))\n"
            "sys.exit(int(os.environ.get('GH_EXIT', '0')))\n"
        )
        gh.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{self.root}:{os.environ['PATH']}",
            "GH_LOG": str(self.log),
            "GITHUB_REPOSITORY": "xflops/flame",
        }

    def decide(self, mode, expected, current="", status=0):
        """Run the production guard against a controlled GitHub response."""
        return subprocess.run(
            ["bash", str(SCRIPT), mode, expected],
            env={**self.env, "GH_RESULT": current, "GH_EXIT": str(status)},
            capture_output=True,
            text=True,
        )

    def test_current_main_can_promote_images(self):
        result = self.decide("main", "new-sha", "new-sha")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "true\n")
        self.assertEqual(
            json.loads(self.log.read_text()),
            ["api", "repos/xflops/flame/commits/main", "--jq", ".sha"],
        )

    def test_stale_main_rerun_cannot_promote_images(self):
        result = self.decide("main", "old-sha", "new-sha")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "false\n")

    def test_latest_release_can_replace_default_installers(self):
        result = self.decide("release", "v1.2.3", "v1.2.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "true\n")
        self.assertEqual(
            json.loads(self.log.read_text()),
            ["api", "repos/xflops/flame/releases/latest", "--jq", ".tag_name"],
        )

    def test_old_release_recovery_keeps_default_installers(self):
        result = self.decide("release", "v1.2.2", "v1.2.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "false\n")

    def test_prerelease_never_replaces_defaults_or_requires_latest_release(self):
        result = self.decide("release", "v1.2.4-rc1", status=1)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "false\n")
        self.assertFalse(self.log.exists())

    def test_api_failure_never_authorizes_publication(self):
        for mode in ["main", "release"]:
            with self.subTest(mode=mode):
                result = self.decide(mode, "expected", "expected", status=1)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("true", result.stdout)

    def test_missing_identity_never_authorizes_publication(self):
        for current in ["", "null"]:
            with self.subTest(current=current):
                result = self.decide("main", current, current)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("true", result.stdout)


if __name__ == "__main__":
    unittest.main()
