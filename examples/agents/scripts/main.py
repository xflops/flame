"""Run Python and shell code remotely with Flame agent sessions."""

from flamepy import agent


def main():
    with agent.open_session(language="python") as session:
        output = session.run_code("print(sum(range(1, 101)))")
        print(f"Python> {output.text().strip()}")

    with agent.open_session(language="shell", runtime="bash") as session:
        output = session.run_code(
            'total=0; for ((i=1; i<=100; i++)); do ((total+=i)); done; echo "$total"'
        )
        print(f"Shell> {output.text().strip()}")


if __name__ == "__main__":
    main()
