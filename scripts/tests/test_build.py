import hashlib
import shutil
import subprocess
import tarfile
import unittest
from pathlib import Path

from helpers import BINARIES, SCRIPTS, TOOLS, ScriptCase, write_executable

FAKE_CARGO = """#!/bin/sh
echo "cargo $*" >> "$FAKE_LOG"
[ -z "${FAKE_CARGO_FAIL:-}" ] || { echo "error: build failed" >&2; exit 101; }
mkdir -p target/release
for b in %s; do
  [ "$b" = "${FAKE_MISSING:-}" ] && continue
  printf '#!/bin/sh\\necho %%s\\n' "$b" > "target/release/$b"; chmod 755 "target/release/$b"
done
""" % " ".join(BINARIES)


class BuildTests(ScriptCase):
    def setUp(self):
        super().setUp()
        self.src = self.root / "src"
        (self.src / "scripts").mkdir(parents=True)
        for name in ("build.sh", "inline.sh", "setup.sh", "sdk.sh"):
            shutil.copy(SCRIPTS / name, self.src / "scripts" / name)
        shutil.copy(SCRIPTS / "flmenv.sh.in", self.src / "scripts/flmenv.sh.in")
        shutil.copytree(SCRIPTS / "lib", self.src / "scripts/lib")
        (self.src / "common/migrations/sqlite").mkdir(parents=True)
        (self.src / "common/Cargo.toml").write_text('[package]\nname = "common"\nversion = "0.7.0"\n')
        (self.src / "common/migrations/sqlite/001.sql").write_text("create table t(x);")
        (self.src / "config/applications").mkdir(parents=True)
        (self.src / "config/applications/flmexec.yaml").write_text("metadata:\n  name: flmexec\n")
        (self.src / "sdk/python").mkdir(parents=True)
        (self.src / "LICENSE").write_text("license")
        write_executable(self.tools / "cargo", FAKE_CARGO)
        self.scripts_dir = self.src / "scripts"
        self.out = self.root / "dist"

    def build(self, *args, env=None):
        return self.run_script("build.sh", "--output", str(self.out), *args, env=env)

    def ok(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_writes_the_package_archive_and_tools(self):
        self.ok(self.build())

        names = sorted(p.name for p in self.out.iterdir())
        suffix = "v0.7.0-linux-amd64"
        self.assertEqual(
            names,
            sorted(
                ["package", f"flame-{suffix}.tar.gz", f"flame-{suffix}.tar.gz.sha256"]
                + [f"{t}-{suffix}" for t in TOOLS] + [f"{t}-{suffix}.sha256" for t in TOOLS]
            ),
        )

    def test_the_package_layout(self):
        self.ok(self.build("--tag", "v1.0.0-rc1"))

        pkg = self.out / "package"
        for binary in BINARIES + ["uv"]:
            self.assertEqual((pkg / "bin" / binary).stat().st_mode & 0o111, 0o111, binary)
        wheels = list((pkg / "wheels").iterdir())
        self.assertEqual([w.name for w in wheels], ["flamepy-0.7.0-py3-none-any.whl"])
        self.assertTrue((pkg / "config/migrations/sqlite/001.sql").is_file())
        self.assertFalse((pkg / "common").exists())
        self.assertTrue((pkg / "config/applications/flmexec.yaml").is_file())
        self.assertEqual((pkg / "sbin/flmenv.sh.in").read_text(), (SCRIPTS / "flmenv.sh.in").read_text())
        self.assertEqual((pkg / "LICENSE").read_text(), "license")
        self.assertEqual((pkg / "VERSION").read_text(), "v1.0.0-rc1\n")
        self.assertEqual((pkg / "PACKAGE_FORMAT").read_text(), "1\n")
        self.assertTrue((self.out / "flame-v1.0.0-rc1-linux-amd64.tar.gz").is_file())

    def test_the_package_carries_self_contained_setup_and_sdk_scripts_in_sbin(self):
        self.ok(self.build("--tag", "v1.0.0-rc1"))

        for name in ("setup.sh", "sdk.sh"):
            script = self.out / "package/sbin" / name
            self.assertEqual(script.stat().st_mode & 0o111, 0o111, name)
            text = script.read_text()
            self.assertNotIn("#@include", text)
            self.assertIn("detect_platform()", text)
            # the package's scripts default to the release they came in
            self.assertIn('FLAME_DEFAULT_VERSION="v1.0.0-rc1"', text)
            self.assertEqual(subprocess.run(["sh", "-n", str(script)]).returncode, 0)

    def test_the_package_installs_itself_with_its_own_setup_sh(self):
        self.ok(self.build())
        prefix = self.root / "flame"

        # no --package, no network: the package the script is in
        result = subprocess.run(
            ["sh", str(self.out / "package/sbin/setup.sh"), "--prefix", str(prefix), "--join", "--no-systemd"],
            env=self.env, capture_output=True, text=True,
        )

        self.ok(result)
        self.assertIn("Using release package", result.stdout)
        self.assertTrue((prefix / "bin/flame-executor-manager").is_file())
        self.assertTrue((prefix / "sbin/flmenv.sh").is_file())

    def test_the_package_installs_the_client_with_its_own_sdk_sh(self):
        self.ok(self.build())
        prefix = self.root / "client"

        result = subprocess.run(
            ["sh", str(self.out / "package/sbin/sdk.sh"), "--prefix", str(prefix)],
            env=self.env, capture_output=True, text=True,
        )

        self.ok(result)
        for tool in TOOLS:
            self.assertTrue((prefix / "bin" / tool).is_file(), tool)
        # the SDK comes from the package's wheel, not from PyPI
        installs = [c for c in self.calls() if c.startswith("uv pip install")]
        self.assertEqual(len(installs), 4)
        for install in installs:
            self.assertTrue(install.endswith("flamepy-0.7.0-py3-none-any.whl"), install)
            self.assertNotIn("flamepy==", install)

    def test_the_archive_has_one_top_directory_with_the_package(self):
        self.ok(self.build())

        with tarfile.open(self.out / "flame-v0.7.0-linux-amd64.tar.gz") as tar:
            members = tar.getmembers()
            names = {m.name for m in members}
            self.assertEqual({n.split("/")[0] for n in names}, {"flame-v0.7.0-linux-amd64"})
            self.assertIn("flame-v0.7.0-linux-amd64/bin/flame-session-manager", names)
            self.assertIn("flame-v0.7.0-linux-amd64/PACKAGE_FORMAT", names)
            self.assertNotIn("flame-v0.7.0-linux-amd64/wheels/.gitignore", names)
            session = next(m for m in members if m.name.endswith("bin/flame-session-manager"))
            self.assertTrue(session.mode & 0o111)

    def test_checksums_match_the_files(self):
        self.ok(self.build())

        sums = sorted(self.out.glob("*.sha256"))
        self.assertEqual(len(sums), 5)
        for sha in sums:
            digest, name = sha.read_text().split()
            self.assertEqual(name, sha.name[: -len(".sha256")])
            self.assertEqual(digest, hashlib.sha256((self.out / name).read_bytes()).hexdigest())

    def test_platform_names_follow_the_target(self):
        self.ok(self.build(env={"FLAME_ARCH": "arm64"}))

        self.assertTrue((self.out / "flame-v0.7.0-linux-arm64.tar.gz").is_file())
        self.assertTrue((self.out / "flmctl-v0.7.0-linux-arm64").is_file())

    def test_a_non_empty_output_needs_force(self):
        self.out.mkdir()
        (self.out / "old").write_text("x")

        result = self.build()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--force", result.stderr)
        self.assertEqual([p.name for p in self.out.iterdir()], ["old"])
        self.ok(self.build("--force"))
        self.assertFalse((self.out / "old").exists())

    def test_a_failed_build_writes_nothing(self):
        result = self.build(env={"FAKE_CARGO_FAIL": "1"})

        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.out.exists())

    def test_a_missing_binary_writes_nothing(self):
        result = self.build(env={"FAKE_MISSING": "flmbox"})

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing binary", result.stderr)
        self.assertFalse(self.out.exists())

    def test_builds_the_whole_workspace(self):
        self.ok(self.build())

        self.assertIn("cargo build --release", self.calls())
        self.assertTrue([c for c in self.calls() if c.startswith("uv build --wheel")])

    def test_a_built_package_installs_with_setup_sh(self):
        self.ok(self.build())
        shutil.copy(SCRIPTS / "setup.sh", self.scripts_dir / "setup.sh")
        prefix = self.root / "flame"

        result = self.run_script("setup.sh", "--package", str(self.out / "package"), "--prefix", str(prefix), "--join")

        self.ok(result)
        self.assertTrue((prefix / "bin/flame-executor-manager").is_file())

    def test_the_built_archive_installs_with_setup_sh(self):
        self.ok(self.build())
        shutil.copy(SCRIPTS / "setup.sh", self.scripts_dir / "setup.sh")
        prefix = self.root / "flame"
        archive = self.out / "flame-v0.7.0-linux-amd64.tar.gz"

        result = self.run_script("setup.sh", "--package", str(archive), "--prefix", str(prefix), "--no-systemd")

        self.ok(result)  # the .sha256 beside the archive is verified
        self.assertTrue((prefix / "bin/flame-session-manager").is_file())

    def test_unknown_options_exit_with_2(self):
        self.assertEqual(self.run_script("build.sh", "--bogus").returncode, 2)


class InlineTests(unittest.TestCase):
    def setUp(self):
        import tempfile
        self.out = tempfile.TemporaryDirectory()
        self.addCleanup(self.out.cleanup)
        subprocess.run(["sh", str(SCRIPTS / "inline.sh"), self.out.name], check=True)

    def test_hosted_scripts_are_self_contained(self):
        for name in ("setup.sh", "sdk.sh"):
            text = (Path(self.out.name) / name).read_text()
            self.assertNotIn("#@include", text, name)
            self.assertIn("detect_platform()", text, name)
            self.assertIn("Flame Environment Setup Script", text, name)  # the embedded template
            self.assertEqual(subprocess.run(["sh", "-n", str(Path(self.out.name) / name)]).returncode, 0)

    def test_the_scripts_pass_shellcheck_when_it_is_installed(self):
        if shutil.which("shellcheck") is None:
            self.skipTest("shellcheck is not installed")
        files = [SCRIPTS / n for n in ("build.sh", "setup.sh", "sdk.sh", "inline.sh", "lib/common.sh")]
        result = subprocess.run(["shellcheck", "-s", "sh", "-e", "SC1090,SC1091,SC2034", *map(str, files)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_the_scripts_run_under_dash_when_it_is_installed(self):
        dash = shutil.which("dash")
        if dash is None:
            self.skipTest("dash is not installed")
        for name in ("setup.sh", "sdk.sh"):
            result = subprocess.run([dash, str(Path(self.out.name) / name), "--help"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, name + result.stderr)


if __name__ == "__main__":
    unittest.main()
