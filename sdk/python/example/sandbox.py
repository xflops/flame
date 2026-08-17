#!/usr/bin/env python3
"""
Example usage of the Flame Sandbox API.
"""

from flamepy.tools import Sandbox, SandboxAttr


def main():
    attr = SandboxAttr(language="python")
    with Sandbox.create(attr) as sb:
        result = sb.run_code("print(1 + 2)")
        print(result.text())

        other = Sandbox.open(sb.sandbox_id)
        print(other.run_code("print('ready')").text())

    attr = SandboxAttr(language="shell", runtime="bash")
    with Sandbox.create(attr) as sb:
        result = sb.run_code("echo hello")
        print(result.text())


if __name__ == "__main__":
    main()
