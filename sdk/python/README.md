# Flame Python SDK

Python SDK for the Flame, a distributed system for Agentic AI.

For the user guide, see [Flame Python SDK](../../docs/sdk/python.md). For the detailed API surface, see [API.md](docs/API.md).

## Installation

```bash
pip install flamepy
```

## Quick Start

```python
import flamepy

def main():
    # Create a session with the application, e.g. Agent
    session = flamepy.create_session("flmping")
    
    # Create and run a task
    resp = session.invoke(b"task input data")

    # Handle the output of task
    print(resp)

    # Close session
    session.close()

if __name__ == "__main__":
    main()
```

## Verify an App Cluster

After configuring `~/.flame/flame.yaml` for a running cluster with session manager, object cache, executor manager, and the `flmrun` template application, run:

```bash
uv run --project e2e python -m e2e.app
```

Run the command from the repository root. The check packages a tiny temporary
project and verifies App function calls, `ObjectFuture` chaining, and a stateful
instance service. Use `--json` for machine-readable output and
`--python-version 3.12` to verify a specific executor Python runtime.

For application services, initialize the process-wide application before
importing modules that declare services:

```python
import flamepy.app as app

app.init("my-app")

# my_project.services declares services with @app.service().
from my_project import services  # noqa: E402

result = services.run("input")
app.destroy()
```

## API Reference

### Session

Represents a computing session with application, e.g. Agent, Tools.

```python
# Create a session
session = flamepy.create_session("my-app")

# Close a session
session.close()
```

### Task

Represents individual computing tasks within a session.

```python
from concurrent.futures import wait

# Run a task synchronously (blocks until complete)
result = session.invoke(b"input data")

# Run a task and receive a Future after CreateTask succeeds
future = session.run(b"input data")
result = future.result()  # Wait for completion

# Submit multiple tasks without waiting for each CreateTask response
futures = [session.submit(f"input {i}".encode()) for i in range(10)]
wait(futures)  # Wait for all tasks to complete
results = [f.result() for f in futures]

# Create a task and get task object
task = session.create_task(b"input data")

# Get task status
task = session.get_task(task.id)

# Watch task progress
for update in session.watch_task(task.id):
    print(f"Task state: {update.state}")
    if update.is_completed():
        break
```

## Error Handling

The SDK provides custom exception types for different error scenarios:

```python
from flamepy import FlameError, FlameErrorCode

try:
    session = flamepy.create_session("flmping")
except FlameError as e:
    if e.code == FlameErrorCode.INVALID_CONFIG:
        print("Configuration error:", e.message)
    elif e.code == FlameErrorCode.INVALID_STATE:
        print("State error:", e.message)
```

## Development

To set up the development environment:

```bash
# Clone the repository
git clone https://github.com/xflops/flame.git
cd flame/sdk/python

# Install in development mode
# Note: Use --no-build-isolation if you have setuptools>=61.0 installed
python3 -m pip install -e . --user --no-build-isolation

# Or build and install the wheel
python3 -m build --wheel --no-isolation
python3 -m pip install dist/flamepy-0.6.0-py3-none-any.whl --user

# Install development dependencies
pip install -e .[dev]

# Run tests
pytest

# Format code
ruff format

# Type checking
mypy flamepy/
```

### Requirements

- Python >= 3.9
- setuptools >= 61.0 (for building from source)

### Troubleshooting

If you encounter issues with the package name showing as "UNKNOWN":

1. Make sure you have `setuptools>=61.0` installed:
   ```bash
   python3 -m pip install --user --upgrade "setuptools>=61.0"
   ```

2. Clean any stale build artifacts:
   ```bash
   rm -rf dist build src/flamepy.egg-info *.egg-info
   ```

3. Use `--no-build-isolation` flag when installing:
   ```bash
   python3 -m pip install . --user --no-build-isolation
   ```
