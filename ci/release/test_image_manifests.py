"""Exercise publication gates without building or pushing any images."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("image-manifests.sh")
IMAGES = [
    "flame-session-manager",
    "flame-object-cache",
    "flame-executor-manager",
    "flame-console",
    "flame-test",
    "flame-instance-runtime",
]


class ImageManifestsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "calls.jsonl"
        docker = self.root / "docker"
        docker.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['CALL_LOG'], 'a') as f:\n"
            "    f.write(json.dumps(args) + '\\n')\n"
            "if args[:3] == ['buildx', 'imagetools', 'create'] and os.getenv('FAIL_CREATE'):\n"
            "    sys.exit(1)\n"
            "if args[0] == 'run' and os.getenv('FAIL_RUN'):\n"
            "    sys.exit(1)\n"
            "if args[:3] == ['buildx', 'imagetools', 'inspect']:\n"
            "    print('linux/amd64 linux/arm64')\n"
            "if args[:2] == ['image', 'inspect']:\n"
            "    print(os.environ['TEST_ARCH'])\n"
        )
        docker.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{self.root}:{os.environ['PATH']}",
            "CALL_LOG": str(self.log),
            "TEST_ARCH": "amd64",
        }
        for arch, char in [("amd64", "a"), ("arm64", "b")]:
            self.metadata(arch, {image: {"containerimage.digest": "sha256:" + char * 64} for image in IMAGES})

    def metadata(self, arch, data):
        (self.root / f"digests-{arch}.json").write_text(json.dumps(data))

    def run_script(self, mode="merge", value="sha-test", metadata=None):
        return subprocess.run(
            ["bash", str(SCRIPT), mode, "docker.io/xflops", value, str(metadata or self.root)],
            env=self.env,
            capture_output=True,
            text=True,
        )

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_merge_uses_both_immutable_digests_and_verifies_every_image(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(len(calls), 2 * len(IMAGES))
        for image, command in zip(IMAGES, calls[: len(IMAGES)]):
            self.assertEqual(command, [
                "buildx", "imagetools", "create", "--tag", f"docker.io/xflops/{image}:sha-test",
                f"docker.io/xflops/{image}@sha256:" + "a" * 64,
                f"docker.io/xflops/{image}@sha256:" + "b" * 64,
            ])
        self.assertTrue(all(command[:3] == ["buildx", "imagetools", "inspect"] for command in calls[len(IMAGES) :]))

    def test_missing_last_digest_prevents_all_registry_writes(self):
        self.metadata("arm64", {image: {"containerimage.digest": "sha256:" + "b" * 64} for image in IMAGES[:-1]})
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_invalid_digest_prevents_registry_writes(self):
        self.metadata("arm64", {image: {"containerimage.digest": "sha256:invalid"} for image in IMAGES})
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_missing_architecture_file_prevents_registry_writes(self):
        (self.root / "digests-arm64.json").unlink()
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_failed_create_stops_publication(self):
        self.env["FAIL_CREATE"] = "1"
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertEqual(len(self.calls()), 1)

    def test_native_checks_run_binaries_and_sdk_for_each_architecture(self):
        for arch in ["amd64", "arm64"]:
            with self.subTest(arch=arch):
                self.env["TEST_ARCH"] = arch
                result = self.run_script("check", arch, self.root / f"digests-{arch}.json")
                self.assertEqual(result.returncode, 0, result.stderr)
        runs = [command for command in self.calls() if command[0] == "run"]
        self.assertEqual(len(runs), 20)
        sdk = [command for command in runs if "import flamepy" in command[-1]]
        self.assertEqual(len(sdk), 8)

    def test_wrong_native_architecture_fails_before_running_binary(self):
        self.env["TEST_ARCH"] = "arm64"
        self.assertNotEqual(self.run_script("check", "amd64", self.root / "digests-amd64.json").returncode, 0)
        self.assertFalse(any(command[0] == "run" for command in self.calls()))

    def test_failed_native_smoke_check_stops_remaining_images(self):
        self.env["FAIL_RUN"] = "1"
        self.assertNotEqual(self.run_script("check", "amd64", self.root / "digests-amd64.json").returncode, 0)
        self.assertEqual(len(self.calls()), 3)

    def test_unknown_architecture_never_invokes_docker(self):
        self.assertNotEqual(self.run_script("check", "s390x", self.root / "digests-amd64.json").returncode, 0)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
