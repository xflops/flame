# Run Python and shell code

This example uses `flamepy.agent.open_session` to execute Python and shell code
in `flmbox` sandboxes on Flame workers. `run_code` takes the language per call
(`shell` by default) and returns a `CommandResult` with the exit code, stdout and
stderr; `stdout_text()` decodes the output. The Python session selects an image that
has Python with `image="python:3.12-slim"`. Both scripts compute the
sum of integers from 1 to 100. Context managers close the sessions after use.

With Flame running and the Python SDK installed, run in flame-console:

```bash
source /usr/local/flame/sbin/flmenv.sh --python-version 3.12
cd "$FLAME_HOME/examples/agents/scripts"
python3 main.py
```

From a source checkout, use `examples/agents/scripts` as the working directory.
Workers must have Python and Bash available. No model API key is needed.
The example uses Flame's built-in script execution application; there is no
application package to deploy.

Expected output:

```text
Python> 5050
Shell> 5050
```
