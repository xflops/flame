# Sandbox Example

This example uses `flamepy.agent.Sandbox` to run shell commands and Python code inside a gVisor (`runsc`) sandbox, served by the built-in `flmbox` application.

## Requirements

- The `flmbox` application is registered. `setup.sh` registers it on the head node.
- `runsc` is installed on the worker nodes, and `flmbox` runs as root.
- The worker can reach the image registry. The default image is `ubuntu:26.04`.

## Run

From the Flame console:

```bash
cd /opt/examples/sandbox
uv run main.py
uv run main.py --image ubuntu:26.04
```

The script runs a shell command, writes and reads a file, runs Python with stdin, `cwd` and `envs`, submits several tasks at once, shows a nonzero exit code, and suspends and resumes the sandbox.

## Notes

- Files and processes stay in the sandbox for the whole session.
- `suspend()` releases the executor. A resumed sandbox is fresh: process and filesystem state is not kept yet.
- Sandboxes have no network: they cannot reach the Flame control plane, the node or the internet.
- Image pulls are cached on the node by digest, so only the first sandbox of an image waits for the download.

## Files

- `main.py`: the example client.
- `pyproject.toml`: the `uv` project, which uses the local `flamepy`.
