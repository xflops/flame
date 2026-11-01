import hashlib
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from helpers import (
    BINARIES, SCRIPTS, ReleaseServer, ScriptCase, make_archive, make_package, write_checksum,
)


class SetupTests(ScriptCase):
    def setUp(self):
        super().setUp()
        self.package = make_package(self.root / "package")
        self.prefix = self.root / "flame"

    def setup(self, *args, **kwargs):
        return self.run_script(
            "setup.sh", "--package", str(self.package), "--prefix", str(self.prefix), *args, **kwargs
        )

    def ok(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def installed(self):
        return {p.name for p in (self.prefix / "bin").iterdir()}

    # --- the head -------------------------------------------------------------------------------

    def test_head_installs_only_the_session_manager(self):
        self.ok(self.setup())

        self.assertEqual(self.installed(), {"flame-session-manager"})
        self.assertTrue((self.prefix / "conf/flame-cluster.yaml").is_file())
        self.assertTrue((self.prefix / "migrations/sqlite/001.sql").is_file())
        self.assertTrue((self.prefix / "conf/applications/flmexec.yaml").is_file())
        self.assertEqual(list((self.prefix / "lib").iterdir()), [])
        self.assertFalse((self.prefix / "sbin/flmenv.sh").exists())
        self.assertEqual((self.prefix / "data").stat().st_mode & 0o777, 0o700)

    def test_head_writes_a_unit_and_starts_it(self):
        self.ok(self.setup())

        unit = (self.unit_dir / "flame-session-manager.service").read_text()
        self.assertIn(f"ExecStart={self.prefix}/bin/flame-session-manager --config {self.prefix}/conf/flame-cluster.yaml", unit)
        self.assertIn(f"StandardOutput=append:{self.prefix}/logs/fsm.log", unit)
        self.assertNotIn("FLAME_PYTHON_VERSION", unit)
        self.assertEqual(
            [c for c in self.calls() if c.startswith("systemctl")],
            [
                "systemctl daemon-reload",
                "systemctl enable flame-session-manager",
                "systemctl restart flame-session-manager",
                "systemctl is-active flame-session-manager",
            ],
        )

    # --- flame-cluster.yaml: from FLAME_* settings, with defaults ---------------------------------

    def config(self):
        return (self.prefix / "conf/flame-cluster.yaml").read_text()

    def test_the_cluster_config_has_defaults_without_settings(self):
        self.ok(self.setup())

        conf = self.config()
        self.assertIn('name: "flame"', conf)
        self.assertIn('endpoint: "http://127.0.0.1:8080"', conf)
        self.assertIn('resreq: "cpu=1,mem=2g"', conf)
        self.assertIn("    - priority\n    - drf\n    - das\n", conf)
        self.assertIn(f'storage: "fs://{self.prefix}/data"', conf)
        self.assertIn("shims: [host]", conf)
        self.assertIn("max_executors: 128", conf)
        self.assertIn('endpoint: "grpc://127.0.0.1:9090"', conf)
        self.assertIn('advertise: "lo"', conf)
        self.assertIn(f'storage: "{self.prefix}/data/cache"', conf)

    def test_the_cluster_config_takes_settings_from_the_environment(self):
        self.ok(self.setup(env={
            "FLAME_CLUSTER_NAME": "prod", "FLAME_ENDPOINT": "http://10.0.0.5:8080",
            "FLAME_CACHE_ENDPOINT": "grpc://0.0.0.0:9090", "FLAME_CACHE_ADVERTISE": "eth0",
            "FLAME_POLICIES": "priority,das", "FLAME_SHIMS": "host cri", "FLAME_MAX_EXECUTORS": "16",
        }))

        conf = self.config()
        self.assertIn('name: "prod"', conf)
        self.assertIn('endpoint: "http://10.0.0.5:8080"', conf)
        self.assertIn('endpoint: "grpc://0.0.0.0:9090"', conf)
        self.assertIn('advertise: "eth0"', conf)
        self.assertIn("    - priority\n    - das\n", conf)
        self.assertNotIn("drf", conf)
        self.assertIn("shims: [host, cri]", conf)
        self.assertIn("max_executors: 16", conf)

    def test_dotenv_in_the_current_directory_is_read_and_the_environment_wins(self):
        (self.root / ".env").write_text(
            "# a comment\nFLAME_CLUSTER_NAME=\"from-dotenv\"\nexport FLAME_ENDPOINT=http://1.2.3.4:8080\n"
            "FLAME_RESREQ='cpu=2,mem=4g'\nOTHER=ignored\n"
        )

        self.ok(self.setup(env={"FLAME_ENDPOINT": "http://5.6.7.8:8080"}))

        conf = self.config()
        self.assertIn('name: "from-dotenv"', conf)
        self.assertIn('endpoint: "http://5.6.7.8:8080"', conf)
        self.assertNotIn("1.2.3.4", conf)
        self.assertIn('resreq: "cpu=2,mem=4g"', conf)

    def test_a_setting_with_a_quote_is_refused_and_nothing_is_written(self):
        result = self.setup(env={"FLAME_CLUSTER_NAME": 'a"b'})

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FLAME_CLUSTER_NAME", result.stderr)
        self.assertFalse((self.prefix / "bin").exists())

    def test_head_summary_shows_how_to_join_and_install_a_client(self):
        out = self.ok(self.setup()).stdout

        self.assertIn("--join", out)
        self.assertIn("https://xflops.io/flame/sdk.sh", out)

    # --- a worker -------------------------------------------------------------------------------

    def test_worker_installs_the_executor_manager_and_the_cache(self):
        self.ok(self.setup("--join"))

        self.assertEqual(
            self.installed(),
            {"flame-executor-manager", "flame-object-cache", "flmping-service", "flmexec-service", "flmbox", "uv"},
        )
        self.assertFalse((self.prefix / "bin/flame-session-manager").exists())
        self.assertFalse((self.prefix / "bin/flmctl").exists())
        self.assertFalse((self.prefix / "migrations/sqlite/001.sql").exists())
        self.assertTrue((self.prefix / "conf/flame-cluster.yaml").is_file())

    def test_join_takes_no_address_and_the_interface_option_is_gone(self):
        # the head's address and the cache's interface are FLAME_* settings, not options
        for args in (("--join", "10.0.0.5"), ("--advertise-interface", "eth0")):
            with self.subTest(args=args):
                self.assertEqual(self.setup(*args).returncode, 2)

    def test_worker_installs_flamepy_for_the_pythons_that_exist(self):
        result = self.ok(self.setup("--join"))

        self.assertTrue((self.prefix / "lib/python3.12/site-packages/flamepy").is_dir())
        self.assertTrue((self.prefix / "lib/python3.13/site-packages/flamepy").is_dir())
        self.assertFalse((self.prefix / "lib/python3.11").exists())
        self.assertIn("Python 3.11 is not available, skipping", result.stderr)
        self.assertTrue(list((self.prefix / "wheels").glob("*.whl")))

    def test_worker_fails_when_no_python_is_available(self):
        result = self.setup("--join", env={"FAKE_PYTHONS": "2.7"})

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no Python interpreter found", result.stderr)

    def test_a_python_version_can_be_named(self):
        self.ok(self.setup("--join", "--python-version", "3.12"))

        self.assertTrue((self.prefix / "lib/python3.12").is_dir())
        self.assertFalse((self.prefix / "lib/python3.13").exists())

    def test_worker_renders_flmenv_from_the_template(self):
        self.ok(self.setup("--join"))

        env = (self.prefix / "sbin/flmenv.sh").read_text()
        self.assertIn(f'export FLAME_HOME="{self.prefix}"', env)
        self.assertIn('Available Python versions: "3.12" "3.13"', env)
        self.assertNotIn("@PREFIX@", env)
        self.assertNotIn("@PYTHON_VERSIONS@", env)
        self.assertEqual((self.prefix / "sbin/flmenv.sh").stat().st_mode & 0o111, 0o111)
        self.assertEqual(subprocess.run(["bash", "-n", str(self.prefix / "sbin/flmenv.sh")]).returncode, 0)

    def test_executor_unit_names_the_host_python_when_flamepy_is_installed_for_it(self):
        self.ok(self.setup("--join", env={"FAKE_HOST_PYTHON": "3.12"}))

        unit = (self.unit_dir / "flame-executor-manager.service").read_text()
        self.assertIn('Environment="FLAME_PYTHON_VERSION=3.12"', unit)
        self.assertTrue((self.unit_dir / "flame-object-cache.service").read_text().count("MemoryMax=12G"))

    def test_executor_unit_falls_back_to_the_newest_installed_python(self):
        self.ok(self.setup("--join", env={"FAKE_HOST_PYTHON": "3.9"}))

        unit = (self.unit_dir / "flame-executor-manager.service").read_text()
        self.assertIn('Environment="FLAME_PYTHON_VERSION=3.13"', unit)

    def test_worker_starts_the_cache_then_the_executor_manager(self):
        self.ok(self.setup("--join"))

        enabled = [c for c in self.calls() if c.startswith("systemctl enable")]
        self.assertEqual(enabled, ["systemctl enable flame-object-cache", "systemctl enable flame-executor-manager"])

    # --- one machine, rerunning, config ---------------------------------------------------------

    def test_head_then_join_on_one_machine_needs_force_for_the_shared_config(self):
        self.ok(self.setup())

        self.assertNotEqual(self.setup("--join").returncode, 0)
        self.ok(self.setup("--join", "--force"))

        self.assertEqual(
            self.installed() & {"flame-session-manager", "flame-executor-manager", "flame-object-cache"},
            {"flame-session-manager", "flame-executor-manager", "flame-object-cache"},
        )

    def test_an_existing_installation_is_an_error_without_force_and_nothing_changes(self):
        self.ok(self.setup())
        conf = self.prefix / "conf/flame-cluster.yaml"
        conf.write_text("edited\n")
        (self.package / "bin/flame-session-manager").write_text("#!/bin/sh\necho v2\n")
        calls_before = len(self.calls())

        result = self.setup()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no terminal to ask on; use --force", result.stderr)
        self.assertEqual(conf.read_text(), "edited\n")
        self.assertNotIn("echo v2", (self.prefix / "bin/flame-session-manager").read_text())
        self.assertEqual(len(self.calls()), calls_before)  # no systemctl call either

    def answer(self, text):
        path = self.root / "answer"
        path.write_text(text)
        return {"FLAME_TTY": str(path)}

    def test_without_force_it_asks_before_overwriting_and_a_yes_replaces_the_installation(self):
        self.ok(self.setup())
        (self.package / "bin/flame-session-manager").write_text("#!/bin/sh\necho v2\n")

        result = self.setup(env=self.answer("y\n"))

        self.ok(result)
        self.assertIn("Overwrite the existing installation? [y/N]", result.stderr)
        self.assertIn("echo v2", (self.prefix / "bin/flame-session-manager").read_text())
        self.assertEqual(len([c for c in self.calls() if c == "systemctl restart flame-session-manager"]), 2)

    def test_a_no_leaves_the_installation_untouched(self):
        self.ok(self.setup())
        conf = self.prefix / "conf/flame-cluster.yaml"
        conf.write_text("edited\n")
        (self.package / "bin/flame-session-manager").write_text("#!/bin/sh\necho v2\n")
        calls_before = len(self.calls())

        for reply in ("n\n", "\n", "maybe\n", ""):
            with self.subTest(reply=reply):
                result = self.setup(env=self.answer(reply))

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("not overwriting", result.stderr)
                self.assertEqual(conf.read_text(), "edited\n")
                self.assertNotIn("echo v2", (self.prefix / "bin/flame-session-manager").read_text())
                self.assertEqual(len(self.calls()), calls_before)

    def test_force_does_not_ask(self):
        self.ok(self.setup())

        result = self.setup("--force")

        self.ok(result)
        self.assertNotIn("[y/N]", result.stderr)

    def test_a_fresh_install_does_not_ask(self):
        result = self.ok(self.setup(env=self.answer("n\n")))

        self.assertNotIn("[y/N]", result.stderr)

    def test_any_existing_target_is_refused(self):
        for relative in ("bin/flame-session-manager", "conf/applications/flmexec.yaml", "migrations/sqlite/001.sql"):
            with self.subTest(relative=relative):
                shutil.rmtree(self.prefix, ignore_errors=True)
                (self.prefix / relative).parent.mkdir(parents=True)
                (self.prefix / relative).write_text("mine")

                result = self.setup("--no-systemd")

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(relative, result.stderr)
                self.assertEqual((self.prefix / relative).read_text(), "mine")

    def test_an_existing_unit_file_is_refused_without_force(self):
        self.unit_dir.mkdir()
        (self.unit_dir / "flame-session-manager.service").write_text("mine")

        result = self.setup()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("flame-session-manager.service already exists", result.stderr)
        self.assertFalse((self.prefix / "bin").exists())

    def test_force_replaces_binaries_manifests_and_config_and_restarts_the_units(self):
        conf = self.prefix / "conf/flame-cluster.yaml"
        self.ok(self.setup())
        manifest = self.prefix / "conf/applications/flmexec.yaml"
        manifest.write_text("customised\n")
        (self.package / "bin/flame-session-manager").write_text("#!/bin/sh\necho v2\n")

        conf.write_text("edited\n")
        self.ok(self.setup("--force"))

        self.assertIn("echo v2", (self.prefix / "bin/flame-session-manager").read_text())
        self.assertIn('endpoint: "http://127.0.0.1:8080"', conf.read_text())
        self.assertEqual(manifest.read_text(), "metadata:\n  name: flmexec\n")
        restarts = [c for c in self.calls() if c == "systemctl restart flame-session-manager"]
        self.assertEqual(len(restarts), 2)

    def test_no_systemd_writes_no_units_and_calls_nothing(self):
        out = self.ok(self.setup("--no-systemd")).stdout

        self.assertFalse(self.unit_dir.exists())
        self.assertEqual([c for c in self.calls() if c.startswith("systemctl")], [])
        self.assertIn("flame-session-manager --config", out)

    # --- the package ----------------------------------------------------------------------------

    def test_the_directory_an_archive_was_extracted_into_names_the_package(self):
        extracted = self.root / "extracted"
        shutil.copytree(self.package, extracted / "flame-v1.2.3-linux-amd64")

        result = self.run_script("setup.sh", "--package", str(extracted), "--prefix", str(self.prefix), "--no-systemd")

        self.ok(result)
        self.assertTrue((self.prefix / "bin/flame-session-manager").is_file())

    def test_a_directory_that_is_not_a_package_is_rejected(self):
        result = self.run_script("setup.sh", "--package", str(self.root / "tools"), "--prefix", str(self.prefix))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not a release package", result.stderr)

    def test_an_archive_is_extracted_and_its_checksum_verified_when_present(self):
        archive = make_archive(self.package, self.root / "flame-v1.2.3-linux-amd64.tar.gz", "flame-v1.2.3-linux-amd64")
        args = ("--package", str(archive), "--prefix", str(self.prefix), "--no-systemd")

        self.ok(self.run_script("setup.sh", *args))  # a bare archive is trusted

        shutil.rmtree(self.prefix)
        write_checksum(archive)
        self.ok(self.run_script("setup.sh", *args))

        Path(str(archive) + ".sha256").write_text("0" * 64 + "  x\n")
        bad = self.run_script("setup.sh", *args)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("checksum mismatch", bad.stderr)

    def test_an_archive_without_a_package_is_rejected(self):
        junk = self.root / "junk"
        (junk / "d").mkdir(parents=True)
        archive = make_archive(junk, self.root / "junk.tar.gz", "junk")

        result = self.run_script("setup.sh", "--package", str(archive), "--prefix", str(self.prefix))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not hold a release package", result.stderr)

    def test_an_unknown_package_format_is_refused(self):
        make_package(self.root / "future", fmt="2")

        result = self.run_script("setup.sh", "--package", str(self.root / "future"), "--prefix", str(self.prefix))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("format 2", result.stderr)
        self.assertFalse((self.prefix / "bin").exists())

    def test_a_missing_package_path_is_rejected(self):
        result = self.run_script("setup.sh", "--package", "/nonexistent/flame.tar.gz", "--prefix", str(self.prefix))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a directory, an archive, or an http(s) URL", result.stderr)

    # --- downloading ----------------------------------------------------------------------------

    def release(self, tag="v1.2.3"):
        directory = self.root / "release"
        directory.mkdir(exist_ok=True)
        archive = make_archive(self.package, directory / f"flame-{tag}-linux-amd64.tar.gz", f"flame-{tag}-linux-amd64")
        write_checksum(archive)
        server = ReleaseServer(directory, tag)
        self.addCleanup(server.close)
        return directory, server

    def test_a_url_is_downloaded_and_verified(self):
        directory, server = self.release()

        result = self.run_script(
            "setup.sh", "--package", f"{server.url}/flame-v1.2.3-linux-amd64.tar.gz", "--prefix", str(self.prefix), "--no-systemd"
        )

        self.ok(result)
        self.assertTrue((self.prefix / "bin/flame-session-manager").is_file())

    def test_a_url_without_a_checksum_is_refused_and_installs_nothing(self):
        directory, server = self.release()
        (directory / "flame-v1.2.3-linux-amd64.tar.gz.sha256").unlink()

        result = self.run_script(
            "setup.sh", "--package", f"{server.url}/flame-v1.2.3-linux-amd64.tar.gz", "--prefix", str(self.prefix)
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(".sha256", result.stderr)
        self.assertFalse((self.prefix / "bin").exists())

    def test_a_url_with_the_wrong_checksum_installs_nothing(self):
        directory, server = self.release()
        (directory / "flame-v1.2.3-linux-amd64.tar.gz.sha256").write_text("1" * 64 + "  x\n")

        result = self.run_script(
            "setup.sh", "--package", f"{server.url}/flame-v1.2.3-linux-amd64.tar.gz", "--prefix", str(self.prefix)
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse((self.prefix / "bin").exists())

    def test_a_missing_url_fails(self):
        directory, server = self.release()

        result = self.run_script("setup.sh", "--package", f"{server.url}/missing.tar.gz", "--prefix", str(self.prefix))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to download", result.stderr)

    def test_the_release_is_downloaded_by_version(self):
        directory, server = self.release("v9.9.9")

        result = self.run_script(
            "setup.sh", "--version", "v9.9.9", "--prefix", str(self.prefix), "--no-systemd",
            env={"FLAME_BASE_URL": server.url},
        )

        self.ok(result)
        self.assertTrue((self.prefix / "bin/flame-session-manager").is_file())

    def test_the_latest_release_is_found_from_the_redirect(self):
        directory, server = self.release("v4.5.6")

        result = self.run_script(
            "setup.sh", "--prefix", str(self.prefix), "--no-systemd",
            env={"FLAME_LATEST_URL": f"{server.url}/latest", "FLAME_BASE_URL": server.url},
        )

        self.ok(result)
        self.assertIn("Flame v4.5.6", result.stdout)

    def test_a_hosted_script_installs_the_release_it_was_published_with_by_default(self):
        directory, server = self.release("v7.7.7")
        hosted = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, hosted, True)
        subprocess.run(["sh", str(SCRIPTS / "inline.sh"), "--version", "v7.7.7", str(hosted)], check=True)
        self.assertIn('FLAME_DEFAULT_VERSION="v7.7.7"', (hosted / "setup.sh").read_text())

        # no --version, no FLAME_VERSION and no redirect: the baked-in release
        result = subprocess.run(
            ["sh", str(hosted / "setup.sh"), "--prefix", str(self.prefix), "--no-systemd"],
            env={**self.env, "FLAME_BASE_URL": server.url, "FLAME_LATEST_URL": f"{server.url}/nonexistent"},
            capture_output=True, text=True,
        )

        self.ok(result)
        self.assertIn("Flame v7.7.7", result.stdout)

    def test_the_hosted_script_installs_when_piped_from_a_url(self):
        if self.scripts_dir == SCRIPTS:
            self.skipTest("only the inlined copy runs on its own")
        directory, server = self.release()

        result = self.run_script(
            "setup.sh", "--package", f"{server.url}/flame-v1.2.3-linux-amd64.tar.gz", "--prefix", str(self.prefix),
            "--no-systemd", piped=True,
        )

        self.ok(result)
        self.assertTrue((self.prefix / "bin/flame-session-manager").is_file())

    def test_uninstall_is_not_an_option(self):
        # removing Flame is documented in scripts/README.md, not scripted
        result = self.run_script("setup.sh", "--uninstall", "--prefix", str(self.prefix))

        self.assertEqual(result.returncode, 2)

    # --- arguments ------------------------------------------------------------------------------

    def test_unknown_options_exit_with_2(self):
        result = self.run_script("setup.sh", "--worker")

        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)

    def test_the_prefix_must_be_absolute(self):
        result = self.run_script("setup.sh", "--package", str(self.package), "--prefix", "relative")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute", result.stderr)

    def test_help_prints_usage(self):
        result = self.run_script("setup.sh", "--help")

        self.assertEqual(result.returncode, 0)
        self.assertIn("--join ", result.stdout)
        self.assertNotIn("advertise", result.stdout)


class HostedSetupTests(SetupTests):
    """The same tests against the inlined copy that is published."""

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
