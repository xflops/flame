"""Exercise CI uninstall ordering without services or privileged mounts."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[2] / "ci/cri/e2e.sh"


class CriCleanupTest(unittest.TestCase):
    def run_cleanup(self, mounted, unmount_status=0):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            sudo = root / "sudo"
            sudo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['COMMAND_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "command = sys.argv[1]\n"
                "if command == 'mountpoint':\n"
                "    sys.exit(int(os.environ['MOUNT_STATUS']))\n"
                "if command == 'umount':\n"
                "    sys.exit(int(os.environ['UNMOUNT_STATUS']))\n"
            )
            sudo.chmod(0o755)
            log = root / "commands.jsonl"
            prefix = str(root / "prefix with spaces")
            result = subprocess.run(
                ["bash", str(SCRIPT), "uninstall-flame"],
                env={
                    **os.environ,
                    "PATH": f"{root}:{os.environ['PATH']}",
                    "COMMAND_LOG": str(log),
                    "MOUNT_STATUS": "0" if mounted else "1",
                    "UNMOUNT_STATUS": str(unmount_status),
                    "INSTALL_PREFIX": prefix,
                    "HOME": str(root / "home"),
                },
                capture_output=True,
                text=True,
            )
            commands = [json.loads(line) for line in log.read_text().splitlines()]
            return result, commands, prefix

    def test_mounted_netns_is_unmounted_before_prefix_removal(self):
        result, commands, prefix = self.run_cleanup(mounted=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        unmount = ["umount", "--", f"{prefix}/data/flmbox/runsc/null-netns"]
        removal = next(c for c in commands if c[:2] == ["rm", "-rf"])
        self.assertLess(commands.index(unmount), commands.index(removal))
        self.assertEqual(removal[2], prefix)

    def test_absent_mount_still_removes_installation(self):
        result, commands, _ = self.run_cleanup(mounted=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c[0] == "umount" for c in commands))
        self.assertTrue(any(c[:2] == ["rm", "-rf"] for c in commands))

    def test_failed_unmount_prevents_prefix_removal(self):
        result, commands, _ = self.run_cleanup(mounted=True, unmount_status=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[:2] == ["rm", "-rf"] for c in commands))


if __name__ == "__main__":
    unittest.main()
