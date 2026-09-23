# RFE499: Add Session API to flamepy

GitHub issue: https://github.com/xflops/flame/issues/499

## 1. Motivation

**Background:**

`flmexec` already runs ad-hoc Python and shell scripts on Flame executors. The wire contract is a JSON `Script` request and a JSON `ScriptOutput` response. flamepy users still have to assemble that contract by hand:

```python
session = flamepy.create_session("flmexec")
try:
    request = {"language": "python", "runtime": runtime, "code": script, "input": None}
    raw = session.invoke(json.dumps(request).encode("utf-8"))
finally:
    ssn.close()

output = bytes(json.loads(raw)["data"]).decode("utf-8")
```

That pattern shows up in e2e tests and agent examples. It has three problems:

- Session create/close and JSON encode/decode are repeated around every script.
- `flamepy.service.Session` cannot be used with `flmexec` because it cloudpickles the request. `flmexec` expects `FlameMessage` JSON.
- Domain code such as an agent has to import raw Flame types: the core `Session`, `create_session`, `flmexec`, and task JSON.

The purpose of Session is to simplify the Flame API for a domain and keep raw Flame APIs out of that domain. An agent imports `flamepy.agent` for Session. It may import base types such as `ResourceRequirement` and `FlameError` from `flamepy`. It should not create sessions, submit tasks, or name `flmexec`.

`Runner` does not solve this. Runner packages a Python project and registers a new application on `flmrun`. Session runs scripts through the existing `flmexec` application and hides that path.

**Target:**

Add a `Session` API to `flamepy.agent` that:

- Is the only Flame surface an agent needs in order to run remote Python or shell code.
- Uses one keyword-only `open_session(...)` entry point: omitting `ssn_id` creates a session, while providing it reopens an existing one.
- Accepts optional creation settings directly; language defaults to Python and runtime defaults to server selection.
- Does not export or require the core `Session`, `Task`, `create_session`, or the `flmexec` application name.
- Owns one `flmexec` session internally and reuses it across script runs.
- Encodes the `flmexec` request and decodes stdout without exposing JSON or task bytes.
- Supports Python and shell, with optional runtime and stdin.
- Keeps `flmexec` runtime policy on the server. The client does not invent language defaults.
- Leaves a clear extension point for future storage mounts.

Success criteria:

- An agent can run a remote script with `from flamepy.agent import open_session` and does not import the core `Session`, `Task`, `create_session`, or `flmexec`. `ResourceRequirement` and `FlameError` are imported from `flamepy` when needed.
- A still-open session can be opened again by id and keep the same creation settings. `close()` destroys it.
- Existing `flmexec`, Runner, and core Session APIs stay unchanged.
- Unit tests cover codec, validation, create/open, and lifecycle without a cluster.
- Cluster e2e covers python, shell, open, stdin, submit_code, and close.

## 2. Function Specification

**Configuration:**

No new flame.yaml keys, environment variables, or cluster settings.

Session uses the existing default flamepy connection (`FLAME_ENDPOINT` / `~/.flame/flame.yaml`).

The application name is the existing built-in `flmexec`. It is not configurable in v1.

**API:**

Session lives in the `flamepy.agent` module, next to `flamepy.runner` and `flamepy.service`. It is the domain facade for script execution. Core Session/Task types stay in `flamepy` / `flamepy.core`.

```python
from flamepy.agent import Session, SessionOutput, open_session
from flamepy import ResourceRequirement, FlameError
```

`flamepy` exports the `agent` submodule the same way it exports `runner` and `service`. `agent` must not re-export the core `Session`, `Task`, `create_session`, or the `flmexec` application name. Base types such as `ResourceRequirement` and `FlameError` stay on `flamepy`. Agent session types are not re-exported at the `flamepy` top level.

### Creation options

`open_session` accepts keyword-only options:

- `ssn_id`: optional existing session ID. When set, reopen that session.
- `language`: `"python"` or `"shell"`; defaults to `"python"` for new sessions.
- `runtime`: optional interpreter or shell. `None` lets `flmexec` choose its runtime.
- `min_instances`, `max_instances`, `resreq`: passed through when creating a session, with the same defaults as the core `create_session` API.

Creation options are ignored when `ssn_id` is set. The effective settings are stored privately in session `common_data`, restored on reopen, and fixed for the session lifetime. `resreq` uses the existing `flamepy.ResourceRequirement` type and is copied at creation time.

### SessionOutput

Decoded `flmexec` task output:

```python
@dataclass
class SessionOutput:
    data: bytes

    def text(self, encoding: str = "utf-8") -> str:
        return self.data.decode(encoding)
```

`data` is script stdout. `text()` is a convenience decoder; it is not a second wire field.

`flmexec` does not currently return exit code or stderr. Session does not invent them.

### Session

```python
def open_session(
    *,
    ssn_id: str | None = None,
    language: str = "python",
    runtime: str | None = None,
    min_instances: int = 0,
    max_instances: int | None = None,
    resreq: ResourceRequirement | None = None,
) -> Session: ...

class Session:
    def run_code(self, code: str, input: bytes | None = None) -> SessionOutput: ...

    def submit_code(self, code: str, input: bytes | None = None) -> Future[SessionOutput]: ...

    @property
    def attr(self) -> _SessionOptions: ...

    @property
    def id(self) -> str: ...

    def close(self) -> None: ...

    def __enter__(self) -> Session: ...
    def __exit__(self, exc_type, exc, tb) -> None: ...
```

`open_session()` and `open_session(language=..., runtime=..., ...)`:

- Creates a new session when `ssn_id` is omitted.
- Defaults `language` to `"python"`, lowercases it, and accepts only `"python"` or `"shell"`.
- Creates one `flmexec` session through `flamepy.create_session`.
- Flame assigns the session id. That value is `ssn.id`.
- Persists the effective creation settings as JSON in session `common_data` so the ID form can restore them.
- `flmexec` ignores `common_data`. The bytes are for Session clients only.
- Direct `Session(...)` is not part of the API.

`open_session(ssn_id=...)`:

- Reopens an existing session by id.
- Calls the core `open_session(ssn_id)` with no create spec. It does not create a session.
- Requires the session application to be `flmexec`.
- Restores the private creation settings from session `common_data`.
- Raises `FlameError(NOT_FOUND)` if the session does not exist.
- Raises `FlameError(INVALID_ARGUMENT)` if the session is not open. `open_session` maps server `InvalidState` through gRPC `invalid_argument`.
- Raises `FlameError(INVALID_ARGUMENT)` if the session is not `flmexec` or `common_data` does not contain valid agent session settings.

`run_code` / `submit_code`:

- Take only `code` and optional stdin `input`.
- Always encode the `flmexec` request with the session's private language and runtime settings.
- `run_code` is synchronous and maps to the underlying core `Session.invoke`.
- `submit_code` returns a `Future[SessionOutput]` and maps to the underlying core `Session.run`, so one agent session can run scripts in parallel.

Lifecycle:

- `close()` destroys the session by closing the underlying session. A second close is a no-op.
- Context-manager exit calls `close()`.
- After close, `run_code` and `submit_code` raise `FlameError(INVALID_STATE)`.
- After close, `open_session(ssn_id=...)` raises `FlameError(INVALID_ARGUMENT)` from the core `open_session`.
- The ID form only works on a session that is still open. Another client can open the same id while the original handle stays open and has not called `close()`.

Read-only attributes:

- `id`: the Flame session ID.
- `attr`: the immutable effective creation settings. Its concrete type is private and not exported.

### Wire format

Session must speak the existing `FlameMessage` JSON contract. It must not use cloudpickle.

Task request:

```json
{"language":"python","code":"print(1)","input":null}
{"language":"shell","runtime":"zsh","code":"echo ok","input":null}
{"language":"python","code":"...","input":[97,98,99]}
```

Rules:

- `runtime` is omitted when `None`, matching `skip_serializing_if = "Option::is_none"`.
- `input` is always present: `null` or a JSON array of byte values.

Task response:

```json
{"data":[49,10]}
```

`data` is decoded with `bytes(response["data"])`.

`None` from `Session.invoke` becomes `SessionOutput(data=b"")`.

Session `common_data` for the private creation settings:

```json
{
  "language": "python",
  "runtime": null,
  "min_instances": 1,
  "max_instances": null,
  "resreq": null
}
```

`resreq`, when set, is `{"cpu": 1, "memory": 1073741824, "gpu": 0}` with memory in bytes. `null` fields are stored so reopen can reconstruct the same settings.

### Error handling

| Condition | Error |
| --- | --- |
| `create` with language not `python` or `shell` | `FlameError(INVALID_ARGUMENT)` |
| `input` is not `bytes` or `None` | `FlameError(INVALID_ARGUMENT)` |
| `open` on a missing session | `FlameError(NOT_FOUND)` |
| `open` on a closed session | `FlameError(INVALID_ARGUMENT)` |
| Reopen a non-`flmexec` session or invalid settings bytes | `FlameError(INVALID_ARGUMENT)` |
| Use after `close()` | `FlameError(INVALID_STATE)` |
| Session create / task failure | Propagate the existing `FlameError` |
| Response is not valid `flmexec` output JSON | `FlameError(INTERNAL)` |
| `SessionOutput.text()` on non-decodable bytes | `UnicodeDecodeError` |

Unsupported shell runtimes stay a server-side `flmexec` error. The client does not duplicate the supported-shell list.

**CLI:**

None. `flmexec` remains the Rust CLI.

**Other Interfaces:**

No protobuf, REST, or `flame-rs` changes.

**Scope:**

In scope:

- `open_session`, `Session`, and `SessionOutput`.
- Immutable private creation settings persisted in session `common_data`.
- Session ownership, JSON codec, language validation, sync and async run.
- Unit tests, cluster e2e, and API docs.
- One small Python example.

Out of scope:

- Storage / filesystem mounts. Reserved for a later revision; no attr field in v1.
- Changing `flmexec` request or response fields.
- Exit code, stderr, or process-failure mapping. `flmexec` currently returns stdout even when the child exits non-zero.
- Persistent files across `run_code()` calls. Each `flmexec` task uses a fresh temp working directory and deletes it when the task ends.
- Making `flamepy.service.Session` speak `flmexec` JSON.
- A one-shot module helper such as `flamepy.run_script(...)`.
- WASM / extra languages.
- Updating creation settings after create.
- Exposing the core `Session`, `Task`, `create_session`, or the `flmexec` application name through `flamepy.agent`.
- Re-exporting `ResourceRequirement` or `FlameError` from `flamepy.agent`. Callers use `flamepy` for those types.

Limitations:

- Session is a session wrapper, not a long-lived VM. Files, cwd, and process state do not survive across `run_code()` calls.
- Stdout-only output. Binary stdout is available through `SessionOutput.data`.
- Script dependencies remain `flmexec` behavior. Python scripts may use PEP 723 inline metadata because `flmexec` launches them with `uv run`.
- The ID form only works for sessions created by agent `open_session()`, because only those sessions store the private creation settings in `common_data`.

**Feature Interaction:**

Related features:

- `flmexec` application and `Script` / `ScriptOutput` JSON types. Hidden behind Session.
- flamepy core `create_session`, `open_session`, `Session.invoke`, `Session.run`. Used only inside `flamepy.agent`.
- `flamepy.runner`, which remains the packaging API for Python services.
- `flamepy.service`, which remains the typed service-session helper.
- Agent examples such as SRA. They should call `agent.open_session` instead of `create_session("flmexec")`.

Updates required:

- Add `flamepy.agent` and export it from `flamepy` as a submodule.
- Document the API in `sdk/python/docs/API.md`.

Integration points:

```text
user -> agent.open_session(language=..., runtime=...)
     -> persist settings JSON as session common_data
     -> create_session("flmexec")

user -> agent.open_session(ssn_id=id)
     -> core open_session(id)
     -> restore private settings from common_data

user -> Session.run_code/submit_code
     -> encode Script JSON
     -> core Session.invoke/run("flmexec")
     -> flmexec-service
     -> PythonScript / ShellScript
     -> ScriptOutput JSON
     -> Session decodes SessionOutput
```

Compatibility:

- Existing raw `create_session("flmexec")` callers keep working.
- Existing e2e `test_flmexec.py` can keep using the raw API or switch to the agent Session later.
- The short-lived `flamepy.tools.Sandbox` API is removed without aliases or compatibility shims.

Breaking changes: callers of `flamepy.tools.Sandbox` must migrate to the keyword-only `flamepy.agent.open_session` API and use `ssn.id` for the session ID.

## 3. Implementation Detail

**Architecture:**

Session is a thin client adapter. It does not register an application, upload a package, or change executor behavior.

```text
agent.open_session()          --> core Session(flmexec, common_data=settings JSON)
agent.open_session(ssn_id=id) --> core Session + restore settings
Session.run_code              --> Script JSON --> flmexec-service --> SessionOutput
```

**Components:**

- `sdk/python/src/flamepy/agent/__init__.py`
  - export `open_session`, `Session`, and `SessionOutput`
- `sdk/python/src/flamepy/agent/session.py`
  - private creation settings, `SessionOutput`, `Session`
  - private encode/decode helpers
  - language normalization
- `sdk/python/src/flamepy/__init__.py`
  - import and export the `agent` submodule
- `sdk/python/docs/API.md`
  - Agent / Session section
- `sdk/python/tests/test_agent.py`
  - mocked session tests for create, open, codec, and immutability
- `sdk/python/example/agent/session.py`
  - python and shell examples
- `e2e/tests/test_agent.py`
  - cluster tests for python, shell, open, stdin, submit_code, and close

`flmexec` and `flame-rs` are unchanged.

**Data Structures:**

`_SessionOptions` is a private immutable representation of effective creation settings. `SessionOutput` is the client result for `flmexec` stdout. The `flmexec` `Script` JSON request stays a private codec detail.

Private helpers:

- `_encode_script(language, runtime, code, input) -> bytes`
- `_decode_output(raw: bytes | None) -> SessionOutput`
- `_encode_options(attr: _SessionOptions) -> bytes`
- `_decode_options(raw: bytes | None) -> _SessionOptions`

These stay private. Call sites only need `open_session`, `run_code`, and `submit_code`.

**Algorithms:**

`open_session()`:

1. Build private settings from the keyword arguments, defaulting language to `"python"` and runtime to `None`.
2. Lowercase language, validate it, and copy `resreq` so the stored settings do not alias the caller's object.
3. Call core `create_session("flmexec", ...)` with the encoded settings and scaling options.
4. Return `Session` wrapping the core session and private settings.

`open_session(ssn_id=...)`:

1. `session = _open_core_session(ssn_id)`.
2. Reject a non-`flmexec` application.
3. Decode the private settings from `session.common_data()`.
4. Return `Session` wrapping the session and restored settings.

`run_code`:

1. Encode `flmexec` request JSON from `code`, `input`, and the session's private settings.
2. `output = self._session.invoke(payload)`.
3. Decode `SessionOutput`.

`submit_code` is the same path with `self._session.run(payload)` and a wrapper that decodes the future result.

Do not wrap the future in another thread. Decode when the caller reads the result.

**System Considerations:**

- Performance: one session create per `open_session()` without `ssn_id`. Each `run_code` is one Flame task. Reuse the session for repeated scripts and reopen it by ID when needed.
- Scalability: parallel `submit_code` uses the existing session task pool and creation-time scaling options.
- Reliability: session create and task failures use current Flame error paths. Session does not retry.
- Resource usage: no extra local temp directories. `flmexec` already creates a per-script workdir on the executor.
- Security: same trust model as `flmexec`. Session does not add isolation. It only simplifies the client API.
- Observability: log session id and language at debug level. Do not log full script bodies by default.
- Operational: requires the built-in `flmexec` application, already registered with Flame.

**Dependencies:**

- Internal: `flamepy.create_session`, `open_session`, core `Session`, `FlameError`, `ResourceRequirement`. `agent` does not re-export those core types. Callers import `ResourceRequirement` and `FlameError` from `flamepy`.
- External: stdlib `json` and `dataclasses` only.
- No new package dependencies.

## 4. Use Cases

**Example 1: Create a session and run Python**

```python
from flamepy.agent import open_session

with open_session() as ssn:
    result = ssn.run_code("print(1 + 2)")
    print(result.text())  # "3\n"
```

**Example 2: Run a shell script with an explicit runtime**

```python
from flamepy.agent import open_session

with open_session(language="shell", runtime="bash") as ssn:
    result = ssn.run_code("echo hello")
    assert result.text().strip() == "hello"
```

**Example 3: Reopen an existing session**

```python
from flamepy.agent import open_session

ssn = open_session()
other = open_session(ssn_id=ssn.id)
print(other.run_code("print('ready')").text())
ssn.close()  # destroys the session; other cannot be used or reopened
```

**Example 4: Parallel scripts**

```python
from flamepy.agent import open_session

with open_session() as ssn:
    futures = [ssn.submit_code(f"print({i} * {i})") for i in range(4)]
    print([future.result().text().strip() for future in futures])
```

**Example 5: Stdin input**

```python
from flamepy.agent import open_session

with open_session() as ssn:
    result = ssn.run_code("import sys; print(sys.stdin.read().upper())", input=b"flame")
    assert result.text() == "FLAME"
```

**Advanced: future storage mounts**

Not in v1. A later revision may add a create-time-only `mounts` keyword:

```python
# Future, not implemented
open_session(mounts=[Mount(source="s3://bucket/data", target="/data")])
```

v1 must not add a dead `mounts` field. Document the extension only.

## 5. References

**Related Documents:**

- [RFE280 Runner](../RFE280-runner/RFE280-runner.md): packaging API. Session does not package or register applications.
- [RFE352 open-session](../RFE352-open-session-enhancement/FS.md): the core `open_session` is the reopen path used by the ID form.
- [RFE455 flame-rs API](../RFE455-simplify-flame-rs-api/FS.md): `FlameMessage` JSON is the `flmexec` payload contract.

**External References:**

- [uv inline script metadata](https://docs.astral.sh/uv/guides/scripts/): existing `flmexec` Python dependency path.

**Implementation References:**

- `flmexec/src/api/mod.rs`: `Script`, `ScriptOutput`
- `flmexec/src/service.rs`: task entrypoint
- `flmexec/src/script/lang/python.rs`: Python defaults and `uv run`
- `flmexec/src/script/lang/shell.rs`: shell defaults and supported shells
- `e2e/tests/test_flmexec.py`: current raw flamepy usage
- `e2e/tests/test_agent.py`: Agent Session cluster coverage
- `examples/agents/sra/readme.md`: session-reuse agent pattern
- `sdk/python/src/flamepy/core/client.py`: `create_session`, `open_session`, `Session.invoke`, `Session.run`
