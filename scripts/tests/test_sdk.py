import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from helpers import SCRIPTS, TOOLS, ScriptCase, write_checksum, write_executable


class SdkTests(ScriptCase):
    def setUp(self):
        super().setUp()
        self.release = self.root / "release"
        self.release.mkdir()
        for tool in TOOLS:
            asset = self.release / f"{tool}-v0.7.0-linux-amd64"
            write_executable(asset, f"#!/bin/sh\necho {tool}\n")
            write_checksum(asset)
        self.prefix = self.root / "client"
        self.base = f"file://{self.release}"

    def sdk(self, *args, env=None, **kwargs):
        return self.run_script(
            "sdk.sh", "--version", "v0.7.0", "--prefix", str(self.prefix), *args,
            env={"FLAME_BASE_URL": self.base, **(env or {})}, **kwargs,
        )

    def ok(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_installs_the_four_tools_verified(self):
        self.ok(self.sdk())

        for tool in TOOLS:
            path = self.prefix / "bin" / tool
            self.assertEqual(path.stat().st_mode & 0o111, 0o111)
            self.assertIn(tool, path.read_text())
        self.assertEqual(sorted(p.name for p in (self.prefix / "bin").iterdir()), sorted(TOOLS))

    def test_a_wrong_checksum_installs_none_of_the_tools(self):
        (self.release / "flmbench-v0.7.0-linux-amd64.sha256").write_text("0" * 64 + "  x\n")

        result = self.sdk()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual(list((self.prefix / "bin").iterdir()), [])

    def test_a_missing_checksum_installs_none_of_the_tools(self):
        (self.release / "flmping-v0.7.0-linux-amd64.sha256").unlink()

        result = self.sdk()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(".sha256", result.stderr)
        self.assertEqual(list((self.prefix / "bin").iterdir()), [])

    def test_installs_the_matching_flamepy_for_every_python_that_exists(self):
        result = self.ok(self.sdk())

        installs = [c for c in self.calls() if c.startswith("uv pip install")]
        self.assertEqual(len(installs), 4)  # 3.11 to 3.14, tried in turn
        for install in installs:
            self.assertIn(f"--prefix {self.prefix}", install)
            self.assertTrue(install.endswith("flamepy==0.7.0"))
        self.assertTrue((self.prefix / "lib/python3.12/site-packages/flamepy").is_dir())
        self.assertTrue((self.prefix / "lib/python3.13/site-packages/flamepy").is_dir())
        self.assertFalse((self.prefix / "lib/python3.11").exists())
        self.assertIn("Python 3.11 is not available, skipping", result.stderr)

    def test_an_existing_client_is_an_error_without_force_and_nothing_changes(self):
        self.ok(self.sdk())
        (self.prefix / "bin/flmctl").write_text("mine")
        installs_before = len(self.calls())

        result = self.sdk()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("flmctl already exists and there is no terminal to ask on; use --force", result.stderr)
        self.assertEqual((self.prefix / "bin/flmctl").read_text(), "mine")
        self.assertEqual(len(self.calls()), installs_before)

        self.ok(self.sdk("--force"))
        self.assertIn("echo flmctl", (self.prefix / "bin/flmctl").read_text())

    def test_without_force_it_asks_and_a_yes_replaces_the_client_and_a_no_does_not(self):
        self.ok(self.sdk())
        (self.prefix / "bin/flmctl").write_text("mine")
        answer = self.root / "answer"

        answer.write_text("n\n")
        refused = self.sdk(env={"FLAME_TTY": str(answer)})
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("not overwriting", refused.stderr)
        self.assertIn("Overwrite the existing client? [y/N]", refused.stderr)
        self.assertEqual((self.prefix / "bin/flmctl").read_text(), "mine")

        answer.write_text("yes\n")
        self.ok(self.sdk(env={"FLAME_TTY": str(answer)}))
        self.assertIn("echo flmctl", (self.prefix / "bin/flmctl").read_text())

    def test_a_python_version_can_be_named(self):
        self.ok(self.sdk("--python-version", "3.12"))

        self.assertTrue((self.prefix / "lib/python3.12").is_dir())
        self.assertFalse((self.prefix / "lib/python3.13").exists())

    def test_no_python_at_all_is_an_error(self):
        result = self.sdk(env={"FAKE_PYTHONS": "2.7"})

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no Python interpreter found", result.stderr)

    def test_release_candidate_tags_map_to_pep440_versions(self):
        for tool in TOOLS:
            asset = self.release / f"{tool}-v0.7.0-rc3-linux-amd64"
            write_executable(asset, f"#!/bin/sh\necho {tool}\n")
            write_checksum(asset)

        self.ok(self.run_script(
            "sdk.sh", "--version", "v0.7.0-rc3", "--prefix", str(self.prefix), env={"FLAME_BASE_URL": self.base}
        ))

        self.assertTrue([c for c in self.calls() if c.endswith("flamepy==0.7.0rc3")])

    def test_uses_pip_when_uv_is_not_installed(self):
        (self.tools / "uv").unlink()
        write_executable(self.tools / "python3.13", (self.tools / "python3").read_text())
        # keep only the fakes and the basic tools on PATH: no uv
        result = self.sdk("--python-version", "3.13", env={"PATH": f"{self.tools}:/usr/bin:/bin"})

        self.ok(result)
        self.assertTrue([c for c in self.calls() if c.startswith("python3 -m pip install")])
        self.assertTrue((self.prefix / "lib/python3.13/site-packages/flamepy").is_dir())

    def test_pip_skips_the_pythons_that_are_not_installed(self):
        (self.tools / "uv").unlink()

        result = self.sdk(env={"PATH": f"{self.tools}:/usr/bin:/bin"})

        # which Pythons /usr/bin has depends on the machine; 3.11 is the one version the fake tools lack
        self.assertIn("Python 3.11 is not available, skipping", result.stderr)

    def test_renders_flmenv_for_the_python_it_installed_for(self):
        self.ok(self.sdk())

        env = (self.prefix / "sbin/flmenv.sh").read_text()
        self.assertIn(f'export FLAME_HOME="{self.prefix}"', env)
        self.assertIn('Available Python versions: "3.12" "3.13"', env)

    def test_writes_no_flame_yaml_without_an_endpoint_and_has_no_default(self):
        self.ok(self.sdk())

        self.assertFalse((self.prefix / "flame.yaml").exists())

    def test_writes_flame_yaml_from_the_environment(self):
        self.ok(self.sdk(env={"FLAME_ENDPOINT": "http://10.0.0.5:8080", "FLAME_CACHE_ENDPOINT": "grpc://10.0.0.6:9090"}))

        conf = (self.prefix / "flame.yaml").read_text()
        self.assertIn('endpoint: "http://10.0.0.5:8080"', conf)
        self.assertIn('endpoint: "grpc://10.0.0.6:9090"', conf)

    def test_writes_flame_yaml_from_dotenv_without_a_cache_default(self):
        (self.root / ".env").write_text("FLAME_ENDPOINT=http://10.0.0.5:8080\n")

        self.ok(self.sdk())

        conf = (self.prefix / "flame.yaml").read_text()
        self.assertIn('endpoint: "http://10.0.0.5:8080"', conf)
        self.assertNotIn("cache", conf)

    def test_an_existing_flame_yaml_is_asked_about_like_the_rest(self):
        env = {"FLAME_ENDPOINT": "http://10.0.0.5:8080"}
        self.ok(self.sdk(env=env))
        (self.prefix / "flame.yaml").write_text("mine\n")

        self.assertNotEqual(self.sdk(env=env).returncode, 0)
        self.assertEqual((self.prefix / "flame.yaml").read_text(), "mine\n")
        self.ok(self.sdk("--force", env=env))
        self.assertIn("10.0.0.5", (self.prefix / "flame.yaml").read_text())

    def test_the_summary_tells_the_user_to_configure_the_endpoint(self):
        out = self.ok(self.sdk()).stdout

        self.assertIn("~/.flame/flame.yaml", out)
        self.assertIn("FLAME_ENDPOINT", out)
        self.assertIn("source " + str(self.prefix / "sbin/flmenv.sh"), out)

    def test_the_default_prefix_is_in_the_home_directory(self):
        result = self.run_script(
            "sdk.sh", "--version", "v0.7.0", env={"FLAME_BASE_URL": self.base}
        )

        self.ok(result)
        self.assertTrue((Path(self.env["HOME"]) / ".flame/bin/flmctl").is_file())

    def test_the_latest_release_is_found_from_the_redirect(self):
        from helpers import ReleaseServer
        server = ReleaseServer(self.release, "v0.7.0")
        self.addCleanup(server.close)

        result = self.run_script(
            "sdk.sh", "--prefix", str(self.prefix),
            env={"FLAME_LATEST_URL": f"{server.url}/latest", "FLAME_BASE_URL": server.url},
        )

        self.ok(result)
        self.assertIn("Flame v0.7.0", result.stdout)

    def test_a_hosted_script_installs_the_release_it_was_published_with_by_default(self):
        hosted = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, hosted, True)
        subprocess.run(["sh", str(SCRIPTS / "inline.sh"), "--version", "v0.7.0", str(hosted)], check=True)

        result = subprocess.run(
            ["sh", str(hosted / "sdk.sh"), "--prefix", str(self.prefix)],
            env={**self.env, "FLAME_BASE_URL": self.base, "FLAME_LATEST_URL": "http://127.0.0.1:1/none"},
            capture_output=True, text=True,
        )

        self.ok(result)
        self.assertIn("Flame v0.7.0", result.stdout)

    def test_unknown_options_exit_with_2(self):
        result = self.run_script("sdk.sh", "--all")

        self.assertEqual(result.returncode, 2)

    def test_the_hosted_script_installs_when_piped(self):
        if self.scripts_dir == SCRIPTS:
            self.skipTest("only the inlined copy runs on its own")

        self.ok(self.sdk(piped=True))

        self.assertTrue((self.prefix / "bin/flmctl").is_file())


class HostedSdkTests(SdkTests):
    @classmethod
    def setUpClass(cls):
        cls.hosted = tempfile.TemporaryDirectory()
        subprocess.run(["sh", str(SCRIPTS / "inline.sh"), cls.hosted.name], check=True)
        cls.scripts_dir = Path(cls.hosted.name)

    @classmethod
    def tearDownClass(cls):
        cls.hosted.cleanup()


if __name__ == "__main__":
    unittest.main()
