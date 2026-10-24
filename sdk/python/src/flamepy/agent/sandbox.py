"""
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
"""

from __future__ import annotations

import json
import logging
import secrets
import string
import time
from concurrent.futures import Future
from dataclasses import dataclass
from typing import Dict, Optional

from flamepy.core.client import create_session as _create_core_session
from flamepy.core.client import get_session as _get_core_session
from flamepy.core.client import open_session as _open_core_session
from flamepy.core.client import resume_session as _resume_core_session
from flamepy.core.client import suspend_session as _suspend_core_session
from flamepy.core.types import FlameError, FlameErrorCode, ResourceRequirement, SessionState

logger = logging.getLogger(__name__)

_FLMBOX_APP = "flmbox"
_SUPPORTED_LANGUAGES = frozenset({"python", "shell"})
_POLL_INTERVAL = 0.2
_ID_ALPHABET = string.ascii_lowercase + string.digits

_STATE_NAMES = {
    SessionState.OPEN: "open",
    SessionState.SUSPENDING: "suspending",
    SessionState.SUSPENDED: "suspended",
    SessionState.CLOSED: "closed",
}


@dataclass(frozen=True)
class CommandResult:
    """Result of a command or script run inside a sandbox."""

    exit_code: int
    stdout: bytes
    stderr: bytes

    def stdout_text(self) -> str:
        return self.stdout.decode("utf-8", errors="replace")

    def stderr_text(self) -> str:
        return self.stderr.decode("utf-8", errors="replace")

    @property
    def ok(self) -> bool:
        return self.exit_code == 0


def _copy_resreq(resreq: Optional[ResourceRequirement]) -> Optional[ResourceRequirement]:
    if resreq is None:
        return None
    return ResourceRequirement(cpu=resreq.cpu, memory=resreq.memory, gpu=resreq.gpu)


def _generate_id() -> str:
    return "sandbox-" + "".join(secrets.choice(_ID_ALPHABET) for _ in range(10))


def _state_name(state) -> str:
    try:
        return _STATE_NAMES[SessionState(int(state))]
    except (KeyError, ValueError, TypeError) as exc:
        raise FlameError(FlameErrorCode.INTERNAL, f"unknown session state {state!r}") from exc


def _validate_timeout(timeout: Optional[float]) -> None:
    if timeout is not None and (isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout <= 0):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "timeout must be a positive number or None")


def _encode_run(cmd: str, cwd: Optional[str], envs: Optional[Dict[str, str]], timeout: Optional[float]) -> bytes:
    if not isinstance(cmd, str) or not cmd:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "cmd must be a non-empty string")
    _validate_timeout(timeout)
    payload = {"kind": "run", "cmd": cmd, "cwd": cwd, "envs": None if envs is None else dict(envs), "timeout": timeout}
    return json.dumps(payload).encode("utf-8")


def _encode_code(code: str, language: str, input_data: Optional[bytes], timeout: Optional[float]) -> bytes:
    if not isinstance(code, str):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "code must be a string")
    if not isinstance(language, str) or language.strip().lower() not in _SUPPORTED_LANGUAGES:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, f"language must be 'python' or 'shell', got {language!r}")
    if input_data is not None and not isinstance(input_data, bytes):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "input must be bytes or None")
    _validate_timeout(timeout)
    payload = {
        "kind": "run_code",
        "code": code,
        "language": language.strip().lower(),
        "input": None if input_data is None else list(input_data),
        "timeout": timeout,
    }
    return json.dumps(payload).encode("utf-8")


def _decode_result(raw: Optional[bytes]) -> CommandResult:
    try:
        payload = json.loads(raw.decode("utf-8"))
        return CommandResult(
            exit_code=int(payload["exit_code"]),
            stdout=bytes(payload["stdout"]),
            stderr=bytes(payload["stderr"]),
        )
    except Exception as exc:
        raise FlameError(FlameErrorCode.INTERNAL, "response is not valid flmbox output JSON") from exc


class Sandbox:
    """A sandboxed container environment backed by the ``flmbox`` application."""

    def __init__(self, session):
        self._session = session
        self._id = session.id
        self._closed = False

    @classmethod
    def create(cls, *, image: Optional[str] = None, resreq: Optional[ResourceRequirement] = None) -> "Sandbox":
        """Create a new sandbox. ``image`` defaults to the service default image."""
        if image is not None and (not isinstance(image, str) or not image):
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "image must be a non-empty string or None")
        common_data = json.dumps({"image": image}).encode("utf-8")
        session = _create_core_session(
            _FLMBOX_APP,
            common_data=common_data,
            session_id=_generate_id(),
            min_instances=1,
            max_instances=1,
            batch_size=1,
            resreq=_copy_resreq(resreq),
        )
        return cls(session)

    @classmethod
    def connect(cls, sandbox_id: str) -> "Sandbox":
        """Attach to an existing sandbox by id."""
        if not isinstance(sandbox_id, str) or not sandbox_id:
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "sandbox_id must be a non-empty string")
        session = _open_core_session(sandbox_id)
        if session.application != _FLMBOX_APP:
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, f"session {session.id!r} is not a sandbox")
        return cls(session)

    @property
    def id(self) -> str:
        return self._id

    @property
    def state(self) -> str:
        """Fresh state from the server: open, suspending, suspended or closed."""
        return _state_name(_get_core_session(self._id).state)

    def run(self, cmd: str, *, cwd: Optional[str] = None, envs: Optional[Dict[str, str]] = None, timeout: Optional[float] = None) -> CommandResult:
        """Run a shell command and wait for its result. A nonzero exit is a normal result."""
        return self.submit(cmd, cwd=cwd, envs=envs, timeout=timeout).result()

    def submit(self, cmd: str, *, cwd: Optional[str] = None, envs: Optional[Dict[str, str]] = None, timeout: Optional[float] = None) -> "Future[CommandResult]":
        payload = _encode_run(cmd, cwd, envs, timeout)
        return self._submit(payload)

    def run_code(self, code: str, *, language: str = "python", input: Optional[bytes] = None, timeout: Optional[float] = None) -> CommandResult:
        """Run a ``python`` or ``shell`` script with optional stdin bytes."""
        return self.submit_code(code, language=language, input=input, timeout=timeout).result()

    def submit_code(self, code: str, *, language: str = "python", input: Optional[bytes] = None, timeout: Optional[float] = None) -> "Future[CommandResult]":
        payload = _encode_code(code, language, input, timeout)
        return self._submit(payload)

    def suspend(self, *, timeout: float = 60.0) -> None:
        """Suspend the sandbox and wait until it is SUSPENDED.

        The suspend signal is asynchronous: the session first becomes
        ``suspending`` and reaches ``suspended`` once its executors are released.
        Raises ``FlameError(INTERNAL)`` if that does not happen within ``timeout``.
        """
        self._ensure_not_closed()
        _suspend_core_session(self._id)
        self._wait_for(SessionState.SUSPENDED, timeout)

    def resume(self, *, timeout: float = 60.0) -> None:
        """Resume a suspended sandbox and wait until it is OPEN.

        Phase 1 does NOT preserve process or filesystem state: a resumed
        sandbox starts fresh.
        """
        self._ensure_not_closed()
        _resume_core_session(self._id)
        self._wait_for(SessionState.OPEN, timeout)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        session = self._session
        self._session = None
        if session is not None:
            session.close()

    def _wait_for(self, target: SessionState, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while True:
            state = SessionState(int(_get_core_session(self._id).state))
            if state == target:
                return
            if state == SessionState.CLOSED:
                raise FlameError(FlameErrorCode.INVALID_STATE, f"sandbox {self._id!r} is closed")
            if time.monotonic() >= deadline:
                raise FlameError(
                    FlameErrorCode.INTERNAL,
                    f"sandbox {self._id!r} did not become {_STATE_NAMES[target]} within {timeout}s (state: {_STATE_NAMES[state]})",
                )
            time.sleep(_POLL_INTERVAL)

    def _ensure_not_closed(self) -> None:
        if self._closed or self._session is None:
            raise FlameError(FlameErrorCode.INVALID_STATE, "sandbox is closed")

    def _ensure_open(self) -> None:
        self._ensure_not_closed()
        state = self.state
        if state != "open":
            raise FlameError(FlameErrorCode.INVALID_STATE, f"sandbox {self._id!r} is {state}, not open")

    def _submit(self, payload: bytes) -> "Future[CommandResult]":
        self._ensure_open()
        raw_future = self._session.submit(payload)
        mapped: Future = Future()

        def complete(done: Future) -> None:
            try:
                mapped.set_result(_decode_result(done.result()))
            except Exception as exc:
                mapped.set_exception(exc)

        register = getattr(raw_future, "_add_internal_callback", raw_future.add_done_callback)
        register(complete)
        return mapped

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()
