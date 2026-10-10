# Run Python and shell code

This example uses `flamepy.agent.open_session` to execute Python and Bash code
on Flame workers. Each session fixes its language and runtime; `run_code`
returns remote stdout through `SessionOutput.text()`. Both scripts compute the
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
