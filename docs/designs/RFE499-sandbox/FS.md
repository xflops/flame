# RFE499: Add Sandbox API to flamepy

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
    session.close()

output = bytes(json.loads(raw)["data"]).decode("utf-8")
```

That pattern shows up in e2e tests and agent examples. It has three problems:

- Session create/close and JSON encode/decode are repeated around every script.
- `flamepy.service.Session` cannot be used with `flmexec` because it cloudpickles the request. `flmexec` expects `FlameMessage` JSON.
- Domain code such as an agent has to import raw Flame types: `Session`, `create_session`, `flmexec`, and task JSON.

The purpose of Sandbox is to simplify the Flame API for a domain and keep raw Flame APIs out of that domain. An agent imports `flamepy.tools` for Sandbox. It may import base types such as `ResourceRequirement` and `FlameError` from `flamepy`. It should not create sessions, submit tasks, or name `flmexec`.

`Runner` does not solve this. Runner packages a Python project and registers a new application on `flmrun`. Sandbox runs scripts through the existing `flmexec` application and hides that path.

**Target:**

Add a `Sandbox` API to `flamepy.tools` that:

- Is the only Flame surface an agent needs in order to run remote Python or shell code.
- Creates a new sandbox with `Sandbox.create(attr)` and reopens an existing one with `Sandbox.open(id)`.
- Takes an immutable `SandboxAttr` at create time. The attr cannot be changed later.
- Does not export or require `Session`, `Task`, `create_session`, or the `flmexec` application name.
- Owns one `flmexec` session internally and reuses it across script runs.
- Encodes the `flmexec` request and decodes stdout without exposing JSON or task bytes.
- Supports Python and shell, with optional runtime and stdin.
- Keeps `flmexec` runtime policy on the server. The client does not invent language defaults.
- Leaves a clear extension point for future storage mounts.

Success criteria:

- An agent can run a remote script with `from flamepy.tools import Sandbox, SandboxAttr` and does not import `Session`, `Task`, `create_session`, or `flmexec`. `ResourceRequirement` and `FlameError` are imported from `flamepy` when needed.
- A still-open sandbox can be opened again by id and keep the same `SandboxAttr`. `close()` destroys it.
- Existing `flmexec`, Runner, and core Session APIs stay unchanged.
- Unit tests cover codec, validation, create/open, and lifecycle without a cluster.
- Cluster e2e covers python, shell, open, stdin, submit_code, and close.

## 2. Function Specification

**Configuration:**

No new flame.yaml keys, environment variables, or cluster settings.

Sandbox uses the existing default flamepy connection (`FLAME_ENDPOINT` / `~/.flame/flame.yaml`).

The application name is the existing built-in `flmexec`. It is not configurable in v1.

**API:**

Sandbox lives in the `flamepy.tools` module, next to `flamepy.runner` and `flamepy.service`. It is the domain facade for script execution. Core Session/Task types stay in `flamepy` / `flamepy.core`.

```python
from flamepy.tools import Sandbox, SandboxAttr, SandboxOutput
from flamepy import ResourceRequirement, FlameError
```

`flamepy` exports the `tools` submodule the same way it exports `runner` and `service`. `tools` must not re-export `Session`, `Task`, `create_session`, or the `flmexec` application name. Base types such as `ResourceRequirement` and `FlameError` stay on `flamepy`. Sandbox types are not re-exported at the `flamepy` top level.

### SandboxAttr

Create-time specification. Frozen after construction. There is no update API.

```python
@dataclass(frozen=True)
class SandboxAttr:
    language: str
    runtime: str | None = None
    min_instances: int = 0
    max_instances: int | None = None
    resreq: ResourceRequirement | None = None
```

Fields:

- `language`: required. `"python"` or `"shell"`, matched case-insensitively and stored in lowercase. Every `run_code` / `submit_code` on this sandbox uses this language. It cannot be changed after create.
- `runtime`: optional interpreter or shell. Python version (`"3.12"`, `"python3.12"`) or shell name/path (`"bash"`, `"/bin/zsh"`). Every `run_code` / `submit_code` on this sandbox uses this runtime. It cannot be changed after create. `None` means omit `runtime` from the request so `flmexec` applies its own default.
- `min_instances`, `max_instances`, `resreq`: passed through to the created session. Defaults match `create_session`: `min_instances=0`, `max_instances=None`, `resreq=None`.

The sandbox id is not part of `SandboxAttr`. `create` assigns it. Callers read `sandbox.sandbox_id` and pass that id to `open`.

`resreq` uses the existing `flamepy.ResourceRequirement` type. Callers import it from `flamepy`.

Immutability:

- `SandboxAttr` is a frozen dataclass. Callers cannot assign fields after construction.
- `Sandbox.create` copies `resreq` into a new `ResourceRequirement` so later mutation of the caller's object does not change `sandbox.attr`.
- `Sandbox.create` stores that snapshot on the session and never writes it again.
- `Sandbox` exposes `attr` as a read-only snapshot. No setters, no `update`.
- `run_code` / `submit_code` do not take `language` or `runtime`. Those values come only from `sandbox.attr`.

### SandboxOutput

Decoded `flmexec` task output:

```python
@dataclass
class SandboxOutput:
    data: bytes

    def text(self, encoding: str = "utf-8") -> str:
        return self.data.decode(encoding)
```

`data` is script stdout. `text()` is a convenience decoder; it is not a second wire field.

`flmexec` does not currently return exit code or stderr. Sandbox does not invent them.

### Sandbox

```python
class Sandbox:
    @classmethod
    def create(cls, attr: SandboxAttr) -> Sandbox: ...

    @classmethod
    def open(cls, sandbox_id: str) -> Sandbox: ...

    def run_code(self, code: str, input: bytes | None = None) -> SandboxOutput: ...

    def submit_code(self, code: str, input: bytes | None = None) -> Future[SandboxOutput]: ...

    @property
    def attr(self) -> SandboxAttr: ...

    @property
    def sandbox_id(self) -> str: ...

    def close(self) -> None: ...

    def __enter__(self) -> Sandbox: ...
    def __exit__(self, exc_type, exc, tb) -> None: ...
```

`create(attr)`:

- Creates a new sandbox. `attr` is required and must include `language`.
- Lowercases `language` and accepts only `"python"` or `"shell"`.
- Creates one `flmexec` session through `flamepy.create_session`.
- Flame assigns the session id. That value is `sandbox.sandbox_id`.
- Persists `attr` as JSON in session `common_data` so `open` can restore it.
- `flmexec` ignores `common_data`. The bytes are for Sandbox clients only.
- Direct `Sandbox(...)` is not part of the API.

`open(sandbox_id)`:

- Reopens an existing sandbox by id.
- Calls `open_session(sandbox_id)` with no create spec. It does not create a session.
- Requires the session application to be `flmexec`.
- Restores `SandboxAttr` from session `common_data`.
- Raises `FlameError(NOT_FOUND)` if the session does not exist.
- Raises `FlameError(INVALID_STATE)` if the session is not open.
- Raises `FlameError(INVALID_ARGUMENT)` if the session is not `flmexec` or `common_data` is not a Sandbox attr.

`run_code` / `submit_code`:

- Take only `code` and optional stdin `input`.
- Always encode the `flmexec` request with `sandbox.attr.language` and `sandbox.attr.runtime`.
- `run_code` is synchronous and maps to `Session.invoke`.
- `submit_code` returns a `Future[SandboxOutput]` and maps to `Session.run`, so one sandbox can run scripts in parallel.

Lifecycle:

- `close()` destroys the sandbox by closing the underlying session. A second close is a no-op.
- Context-manager exit calls `close()`.
- After close, `run_code`, `submit_code`, and `open(sandbox_id)` raise `FlameError(INVALID_STATE)`.
- `open` only works on a sandbox that is still open. Another client can `open` the same id while the original handle stays open and has not called `close()`.

Read-only attributes:

- `attr`: the immutable create-time `SandboxAttr`.
- `sandbox_id`: the sandbox id. Internally this is the Flame session id; domain code should treat it only as a sandbox id.

### Wire format

Sandbox must speak the existing `FlameMessage` JSON contract. It must not use cloudpickle.

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

`None` from `Session.invoke` becomes `SandboxOutput(data=b"")`.

Session `common_data` for `SandboxAttr`:

```json
{
  "language": "python",
  "runtime": null,
  "min_instances": 1,
  "max_instances": null,
  "resreq": null
}
```

`resreq`, when set, is `{"cpu": 1, "memory": 1073741824, "gpu": 0}` with memory in bytes. `null` fields are stored so `open` can reconstruct the same frozen attr.

### Error handling

| Condition | Error |
| --- | --- |
| `create` with language not `python` or `shell` | `FlameError(INVALID_ARGUMENT)` |
| `input` is not `bytes` or `None` | `FlameError(INVALID_ARGUMENT)` |
| `open` on a missing session | `FlameError(NOT_FOUND)` |
| `open` on a closed session | `FlameError(INVALID_STATE)` |
| `open` on a non-`flmexec` session or invalid attr bytes | `FlameError(INVALID_ARGUMENT)` |
| Use after `close()` | `FlameError(INVALID_STATE)` |
| Session create / task failure | Propagate the existing `FlameError` |
| Response is not valid `flmexec` output JSON | `FlameError(INTERNAL)` |
| `SandboxOutput.text()` on non-decodable bytes | `UnicodeDecodeError` |

Unsupported shell runtimes stay a server-side `flmexec` error. The client does not duplicate the supported-shell list.

**CLI:**

None. `flmexec` remains the Rust CLI.

**Other Interfaces:**

No protobuf, REST, or `flame-rs` changes.

**Scope:**

In scope:

- `Sandbox.create`, `Sandbox.open`, `SandboxAttr`, and `SandboxOutput`.
- Immutable create-time attr persisted in session `common_data`.
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
- Updating `SandboxAttr` after create.
- Exposing `Session`, `Task`, `create_session`, or the `flmexec` application name through `flamepy.tools`.
- Re-exporting `ResourceRequirement` or `FlameError` from `flamepy.tools`. Callers use `flamepy` for those types.

Limitations:

- Sandbox is a session wrapper, not a long-lived VM. Files, cwd, and process state do not survive across `run_code()` calls.
- Stdout-only output. Binary stdout is available through `SandboxOutput.data`.
- Script dependencies remain `flmexec` behavior. Python scripts may use PEP 723 inline metadata because `flmexec` launches them with `uv run`.
- `open` only works for sandboxes created by `Sandbox.create`, because only those sessions store `SandboxAttr` in `common_data`.

**Feature Interaction:**

Related features:

- `flmexec` application and `Script` / `ScriptOutput` JSON types. Hidden behind Sandbox.
- flamepy core `create_session`, `open_session`, `Session.invoke`, `Session.run`. Used only inside `flamepy.tools`.
- `flamepy.runner`, which remains the packaging API for Python services.
- `flamepy.service`, which remains the typed service-session helper.
- Agent examples such as SRA. They should call Sandbox instead of `create_session("flmexec")`.

Updates required:

- Add `flamepy.tools` and export it from `flamepy` as a submodule.
- Document the API in `sdk/python/docs/API.md`.

Integration points:

```text
user -> Sandbox.create(attr)
     -> persist attr JSON as session common_data
     -> create_session("flmexec")

user -> Sandbox.open(id)
     -> open_session(id)
     -> restore SandboxAttr from common_data

user -> Sandbox.run_code/submit_code
     -> encode Script JSON
     -> Session.invoke/run("flmexec")
     -> flmexec-service
     -> PythonScript / ShellScript
     -> ScriptOutput JSON
     -> Sandbox decodes SandboxOutput
```

Compatibility:

- No breaking changes.
- Existing raw `create_session("flmexec")` callers keep working.
- Existing e2e `test_flmexec.py` can keep using the raw API or switch to Sandbox later.

Breaking changes: none.

## 3. Implementation Detail

**Architecture:**

Sandbox is a thin client adapter. It does not register an application, upload a package, or change executor behavior.

```text
Sandbox.create(attr) --> Session(flmexec, common_data=attr JSON)
Sandbox.open(id)     --> Session + restore attr
Sandbox.run_code     --> Script JSON --> flmexec-service --> SandboxOutput
```

**Components:**

- `sdk/python/src/flamepy/tools/__init__.py`
  - export `Sandbox`, `SandboxAttr`, `SandboxOutput`
- `sdk/python/src/flamepy/tools/sandbox.py`
  - `SandboxAttr`, `SandboxOutput`, `Sandbox`
  - private encode/decode helpers
  - language normalization
- `sdk/python/src/flamepy/__init__.py`
  - import and export the `tools` submodule
- `sdk/python/docs/API.md`
  - Tools / Sandbox section
- `sdk/python/tests/test_sandbox.py`
  - mocked session tests for create, open, codec, and immutability
- `sdk/python/example/sandbox.py`
  - python and shell examples
- `e2e/tests/test_sandbox.py`
  - cluster tests for python, shell, open, stdin, submit_code, and close

`flmexec` and `flame-rs` are unchanged.

**Data Structures:**

`SandboxAttr` is the create-time contract. `SandboxOutput` is the client result for `flmexec` stdout. The `flmexec` `Script` JSON request stays a private codec detail.

Private helpers:

- `_encode_script(language, runtime, code, input) -> bytes`
- `_decode_output(raw: bytes | None) -> SandboxOutput`
- `_encode_attr(attr: SandboxAttr) -> bytes`
- `_decode_attr(raw: bytes | None) -> SandboxAttr`

These stay private. Call sites only need `create`, `open`, `run_code`, and `submit_code`.

**Algorithms:**

`create`:

1. Lowercase `attr.language` and reject anything other than `"python"` or `"shell"`.
2. Copy `attr.resreq` when set so the stored attr does not alias the caller's object.
3. `session = create_session("flmexec", common_data=_encode_attr(attr), min_instances=attr.min_instances, max_instances=attr.max_instances, resreq=attr.resreq)`.
4. Return `Sandbox` wrapping the session and the normalized, copied attr.

`open`:

1. `session = open_session(sandbox_id)`.
2. Reject a non-`flmexec` application.
3. `attr = _decode_attr(session.common_data())`.
4. Return `Sandbox` wrapping the session and restored attr.

`run_code`:

1. Encode `flmexec` request JSON from `code`, `input`, and `sandbox.attr`.
2. `output = self._session.invoke(payload)`.
3. Decode `SandboxOutput`.

`submit_code` is the same path with `self._session.run(payload)` and a wrapper that decodes the future result.

Do not wrap the future in another thread. Decode when the caller reads the result.

**System Considerations:**

- Performance: one session create per `Sandbox.create`. Each `run_code` is one Flame task. Reuse the session for repeated scripts and across `open`.
- Scalability: parallel `submit_code` uses the existing session task pool and `attr.min_instances` / `attr.max_instances`.
- Reliability: session create and task failures use current Flame error paths. Sandbox does not retry.
- Resource usage: no extra local temp directories. `flmexec` already creates a per-script workdir on the executor.
- Security: same trust model as `flmexec`. Sandbox does not add isolation. It only simplifies the client API.
- Observability: log sandbox id and language at debug level. Do not log full script bodies by default.
- Operational: requires the built-in `flmexec` application, already registered with Flame.

**Dependencies:**

- Internal: `flamepy.create_session`, `open_session`, `Session`, `FlameError`, `ResourceRequirement`. `tools` does not re-export those types. Callers import `ResourceRequirement` and `FlameError` from `flamepy`.
- External: stdlib `json` and `dataclasses` only.
- No new package dependencies.

## 4. Use Cases

**Example 1: Create a sandbox and run Python**

```python
from flamepy.tools import Sandbox, SandboxAttr

attr = SandboxAttr(language="python")
with Sandbox.create(attr) as sb:
    result = sb.run_code("print(1 + 2)")
    print(result.text())  # "3\n"
```

**Example 2: Run a shell script with an explicit runtime**

```python
from flamepy.tools import Sandbox, SandboxAttr

attr = SandboxAttr(language="shell", runtime="bash")
with Sandbox.create(attr) as sb:
    result = sb.run_code("echo hello")
    assert result.text().strip() == "hello"
```

**Example 3: Reopen an existing sandbox**

```python
from flamepy.tools import Sandbox, SandboxAttr

sb = Sandbox.create(SandboxAttr(language="python"))
other = Sandbox.open(sb.sandbox_id)
print(other.attr.language)  # "python"
print(other.run_code("print('ready')").text())
sb.close()  # destroys the sandbox; other cannot be used or reopened
```

**Example 4: Parallel scripts**

```python
from flamepy.tools import Sandbox, SandboxAttr

with Sandbox.create(SandboxAttr(language="python")) as sb:
    futures = [sb.submit_code(f"print({i} * {i})") for i in range(4)]
    print([future.result().text().strip() for future in futures])
```

**Example 5: Stdin input**

```python
from flamepy.tools import Sandbox, SandboxAttr

with Sandbox.create(SandboxAttr(language="python")) as sb:
    result = sb.run_code("import sys; print(sys.stdin.read().upper())", input=b"flame")
    assert result.text() == "FLAME"
```

**Advanced: future storage mounts**

Not in v1. A later revision may add mounts to `SandboxAttr`. Because the attr is immutable, mounts would be create-time only:

```python
# Future, not implemented
SandboxAttr(language="python", mounts=[Mount(source="s3://bucket/data", target="/data")])
```

v1 must not add a dead `mounts` field. Document the extension only.

## 5. References

**Related Documents:**

- [RFE280 Runner](../RFE280-runner/RFE280-runner.md): packaging API. Sandbox does not package or register applications.
- [RFE352 open-session](../RFE352-open-session-enhancement/FS.md): `open_session` is the reopen path under `Sandbox.open`.
- [RFE455 flame-rs API](../RFE455-simplify-flame-rs-api/FS.md): `FlameMessage` JSON is the `flmexec` payload contract.

**External References:**

- [uv inline script metadata](https://docs.astral.sh/uv/guides/scripts/): existing `flmexec` Python dependency path.

**Implementation References:**

- `flmexec/src/api/mod.rs`: `Script`, `ScriptOutput`
- `flmexec/src/service.rs`: task entrypoint
- `flmexec/src/script/lang/python.rs`: Python defaults and `uv run`
- `flmexec/src/script/lang/shell.rs`: shell defaults and supported shells
- `e2e/tests/test_flmexec.py`: current raw flamepy usage
- `e2e/tests/test_sandbox.py`: Sandbox cluster coverage
- `examples/agents/sra/readme.md`: session-reuse agent pattern
- `sdk/python/src/flamepy/core/client.py`: `create_session`, `open_session`, `Session.invoke`, `Session.run`
