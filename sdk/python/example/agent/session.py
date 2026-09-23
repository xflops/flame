#!/usr/bin/env python3
"""
Example usage of the Flame Session API.
"""

from flamepy.agent import open_session


def main():
    with open_session() as ssn:
        result = ssn.run_code("print(1 + 2)")
        print(result.text())

        other = open_session(ssn_id=ssn.id)
        print(other.run_code("print('ready')").text())

    with open_session(language="shell", runtime="bash") as ssn:
        result = ssn.run_code("echo hello")
        print(result.text())


if __name__ == "__main__":
    main()
