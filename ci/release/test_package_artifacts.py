"""Exercise the package publisher's real preflight before release mutations."""

import hashlib
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml

WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/e2e.yaml"


class PackageArtifactsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = yaml.load(WORKFLOW.read_text(), Loader=yaml.BaseLoader)
        cls.steps = {step["name"]: step for step in workflow["jobs"]["publish-package"]["steps"]}

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.files = []
        for arch in ("amd64", "arm64"):
            for name in (f"flame-latest-linux-{arch}.tar.gz", f"flmctl-latest-linux-{arch}"):
                path = self.assets / name
                path.write_bytes(f"built artifact: {name}".encode())
                path.with_name(name + ".sha256").write_text(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}\n")
                self.files.append(path)
        # Keep the test portable while running the production shell preflight.
        # The shim implements the two sha256sum forms used by that step.
        binary = self.root / "bin"
        binary.mkdir()
        sha256sum = binary / "sha256sum"
        sha256sum.write_text(
            "#!/usr/bin/env python3\n"
            "import hashlib, sys\n"
            "from pathlib import Path\n"
            "def digest(name): return hashlib.sha256(Path(name).read_bytes()).hexdigest()\n"
            "args = sys.argv[1:]\n"
            "if args[:2] == ['-c', '--']:\n"
            "    for checksum in args[2:]:\n"
            "        expected, name = Path(checksum).read_text().rstrip('\\n').split('  ', 1)\n"
            "        if digest(name) != expected: sys.exit(1)\n"
            "else:\n"
            "    for name in args: print(digest(name) + '  ' + name)\n"
        )
        sha256sum.chmod(0o755)
        self.env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}", "LATEST_TAG": "latest"}

    def preflight(self, name="Verify complete release assets", **environment):
        return subprocess.run(
            ["bash", "-e", "-o", "pipefail", "-c", self.steps[name]["run"]],
            cwd=self.assets,
            env={**self.env, **environment},
            capture_output=True,
            text=True,
        )

    def test_complete_artifacts_pass(self):
        result = self.preflight()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_package_or_checksum_fails_for_either_architecture(self):
        for artifact in self.files:
            for path in (artifact, artifact.with_name(artifact.name + ".sha256")):
                with self.subTest(file=path.name):
                    original = path.read_bytes()
                    path.unlink()
                    self.assertNotEqual(self.preflight().returncode, 0)
                    path.write_bytes(original)

    def test_corrupt_payload_fails(self):
        self.files[0].write_bytes(b"changed after build")
        self.assertNotEqual(self.preflight().returncode, 0)

    def test_checksum_cannot_name_another_file(self):
        artifact = self.files[0]
        checksum = artifact.with_name(artifact.name + ".sha256")
        checksum.write_text(checksum.read_text().replace(artifact.name, "unexpected-file"))
        self.assertNotEqual(self.preflight().returncode, 0)

    def test_unexpected_visible_or_hidden_asset_fails(self):
        for name in ("unexpected", ".unexpected"):
            with self.subTest(name=name):
                extra = self.assets / name
                extra.write_text("not a release asset")
                self.assertNotEqual(self.preflight().returncode, 0)
                extra.unlink()

    def test_symlinked_asset_fails(self):
        artifact = self.files[0]
        target = self.root / "elsewhere"
        target.write_bytes(artifact.read_bytes())
        artifact.unlink()
        artifact.symlink_to(target)
        self.assertNotEqual(self.preflight().returncode, 0)

    def test_only_the_calling_run_source_is_accepted(self):
        sha = "a" * 40
        good = self.preflight("Verify the source is this CI run", SOURCE_SHA=sha, GITHUB_SHA=sha)
        bad = self.preflight("Verify the source is this CI run", SOURCE_SHA="b" * 40, GITHUB_SHA=sha)
        self.assertEqual(good.returncode, 0, good.stderr)
        self.assertNotEqual(bad.returncode, 0)


if __name__ == "__main__":
    unittest.main()
