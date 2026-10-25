"""Run Python and shell code remotely with Flame agent sessions."""

from flamepy import agent


def main():
    with agent.open_session(image="python:3.12-slim") as session:
        output = session.run_code("print(sum(range(1, 101)))", language="python")
        print(f"Python> {output.stdout_text().strip()}")

    with agent.open_session() as session:
        output = session.run_code(
            'total=0; i=1; while [ "$i" -le 100 ]; do '
            "total=$((total + i)); i=$((i + 1)); done; echo $total"
        )
        print(f"Shell> {output.stdout_text().strip()}")


if __name__ == "__main__":
    main()
