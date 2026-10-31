"""Fixtures for the script tests: fake tools, a fake package, and a local release server."""
import http.server
import os
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent.parent
BINARIES = [
    "flame-session-manager", "flame-executor-manager", "flame-object-cache",
    "flmctl", "flmping", "flmping-service", "flmexec", "flmexec-service", "flmbench", "flmbox",
]
TOOLS = ["flmctl", "flmping", "flmexec", "flmbench"]

FAKE_UV = r"""#!/bin/sh
# Records its arguments; `pip install` creates the site-packages of the requested Python when that
# Python is in $FAKE_PYTHONS, and fails like uv does otherwise; `build` writes a wheel.
echo "uv $*" >> "$FAKE_LOG"
case "$1" in
  pip)
    python=""; prefix=""
    while [ $# -gt 0 ]; do
      case "$1" in --python) python="$2"; shift ;; --prefix) prefix="$2"; shift ;; esac
      shift
    done
    version="${python#python}"
    [ "$version" = 3 ] && version="${FAKE_HOST_PYTHON:-3.13}"
    case " ${FAKE_PYTHONS:-3.12 3.13} " in
      *" $version "*) mkdir -p "$prefix/lib/python$version/site-packages/flamepy"; : > "$prefix/lib/python$version/site-packages/flamepy/__init__.py" ;;
      *) echo "error: No interpreter found for Python $version" >&2; exit 1 ;;
    esac ;;
  build)
    out=""
    while [ $# -gt 0 ]; do case "$1" in --out-dir) out="$2"; shift ;; esac; shift; done
    : > "$out/flamepy-0.7.0-py3-none-any.whl"; : > "$out/.gitignore" ;;
esac
"""

FAKE_PYTHON3 = r"""#!/bin/sh
echo "python3 $*" >> "$FAKE_LOG"
case "$1" in
  -c) echo "${FAKE_HOST_PYTHON:-3.13}" ;;
  -m)
    case "$2" in
      pip)
        [ "$3" = "--version" ] && { echo "pip 99"; exit 0; }
        prefix=""
        while [ $# -gt 0 ]; do case "$1" in --prefix) prefix="$2"; shift ;; esac; shift; done
        v="${FAKE_HOST_PYTHON:-3.13}"
        mkdir -p "$prefix/lib/python$v/site-packages/flamepy" ;;
    esac ;;
esac
"""

FAKE_SYSTEMCTL = """#!/bin/sh
echo "systemctl $*" >> "$FAKE_LOG"
exit 0
"""

FAKE_IP = """#!/bin/sh
echo "default via 10.0.0.1 dev eth9 proto dhcp metric 100"
"""


def write_executable(path, content):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    path.chmod(0o755)


def make_package(directory, version="v1.2.3", fmt="1"):
    """A release package whose binaries print their name."""
    directory = Path(directory)
    for binary in BINARIES:
        write_executable(directory / "bin" / binary, f"#!/bin/sh\necho {binary} {version}\n")
    write_executable(directory / "bin/uv", FAKE_UV)
    (directory / "wheels").mkdir(parents=True, exist_ok=True)
    (directory / "wheels/flamepy-0.7.0-py3-none-any.whl").write_text("wheel")
    (directory / "config/migrations/sqlite").mkdir(parents=True, exist_ok=True)
    (directory / "config/migrations/sqlite/001.sql").write_text("create table t(x);")
    (directory / "config/applications").mkdir(parents=True, exist_ok=True)
    (directory / "config/applications/flmexec.yaml").write_text("metadata:\n  name: flmexec\n")
    (directory / "sbin").mkdir(exist_ok=True)
    shutil.copy(SCRIPTS / "flmenv.sh.in", directory / "sbin/flmenv.sh.in")
    (directory / "LICENSE").write_text("license")
    (directory / "VERSION").write_text(version + "\n")
    (directory / "PACKAGE_FORMAT").write_text(fmt + "\n")
    return directory


def make_archive(package, archive, top):
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(package, arcname=top)
    return Path(archive)


def write_checksum(path):
    import hashlib
    path = Path(path)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    Path(str(path) + ".sha256").write_text(f"{digest}  {path.name}\n")


class ReleaseServer:
    """Serves a directory over HTTP and redirects /latest to /tag/<tag>, like GitHub's releases."""

    def __init__(self, directory, tag="v1.2.3"):
        directory = str(directory)

        class Handler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=directory, **kwargs)

            def do_GET(self):
                if self.path == "/latest":
                    self.send_response(302)
                    self.send_header("Location", f"/tag/{tag}")
                    self.end_headers()
                elif self.path.startswith("/tag/"):
                    self.send_response(200)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                else:
                    super().do_GET()

            def do_HEAD(self):
                self.do_GET()

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class ScriptCase(unittest.TestCase):
    """A temp directory, fake tools first on PATH, and a runner for a script under test."""

    scripts_dir = SCRIPTS  # the tests also run against the inlined (hosted) copies

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tools = self.root / "tools"
        self.log = self.root / "calls.log"
        self.log.write_text("")
        write_executable(self.tools / "uv", FAKE_UV)
        write_executable(self.tools / "python3", FAKE_PYTHON3)
        write_executable(self.tools / "systemctl", FAKE_SYSTEMCTL)
        write_executable(self.tools / "ip", FAKE_IP)
        self.unit_dir = self.root / "units"
        self.env = {
            **os.environ,
            "PATH": f"{self.tools}:{os.environ['PATH']}",
            "FAKE_LOG": str(self.log),
            "FLAME_SYSTEMD_DIR": str(self.unit_dir),
            "FLAME_SYSTEMCTL": str(self.tools / "systemctl"),
            "FLAME_OS": "linux",
            "FLAME_ARCH": "amd64",
            "HOME": str(self.root / "home"),
            # no terminal to ask on, unless a test writes the answer to a file and points this at it
            "FLAME_TTY": str(self.root / "no-tty"),
        }
        for name in ("FLAME_VERSION", "FLAME_BASE_URL", "FLAME_LATEST_URL"):
            self.env.pop(name, None)
        (self.root / "home").mkdir()

    def run_script(self, name, *args, stdin=None, env=None, piped=False):
        script = self.scripts_dir / name
        if piped:
            return subprocess.run(
                ["sh", "-s", "--", *args], input=script.read_text(), env={**self.env, **(env or {})},
                capture_output=True, text=True, cwd=self.root,
            )
        return subprocess.run(
            ["sh", str(script), *args], input=stdin, env={**self.env, **(env or {})},
            capture_output=True, text=True, cwd=self.root,
        )

    def calls(self):
        return self.log.read_text().splitlines()
