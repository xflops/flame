"""
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
"""

import argparse

from flamepy.agent import Sandbox


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Run commands and code in a Flame sandbox"
    )
    parser.add_argument(
        "--image", default=None, help="image to run (default: ubuntu:26.04)"
    )
    args = parser.parse_args()

    with Sandbox.create(image=args.image) as box:
        print(f"sandbox {box.id} is {box.state}")

        # A shell command; a nonzero exit code is a result, not an error.
        result = box.run("cat /etc/os-release | head -n 2; uname -sr")
        print(result.stdout_text())

        # Files live in the sandbox and stay there for the whole session.
        box.run("echo 'hello from the sandbox' > /tmp/note.txt")
        print(box.run("cat /tmp/note.txt").stdout_text().strip())

        # Python code with stdin, working directory and environment variables.
        result = box.run_code(
            "import os, sys\nprint(os.getcwd(), os.environ['GREETING'], sys.stdin.read().upper())",
            language="python",
            cwd="/tmp",
            envs={"GREETING": "hi"},
            input=b"data from the client",
        )
        print(result.stdout_text().strip())

        # Tasks can be submitted together and awaited later.
        futures = [box.submit(f"echo task {i}") for i in range(3)]
        for future in futures:
            print(future.result().stdout_text().strip())

        # A failing command is a normal result.
        result = box.run("exit 3")
        print(f"exit code: {result.exit_code}")

        # Suspend releases the executor; resume starts a fresh sandbox.
        # Process and filesystem state is not kept yet.
        box.suspend()
        print(f"after suspend: {box.state}")
        box.resume()
        print(f"after resume: {box.state}")
        print(box.run("ls /tmp/note.txt || echo note is gone").stdout_text().strip())


if __name__ == "__main__":
    main()
