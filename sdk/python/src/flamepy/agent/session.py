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
from concurrent.futures import Future
from dataclasses import dataclass
from typing import Optional

from flamepy.core.client import _LazyTaskFuture
from flamepy.core.client import create_session as _create_core_session
from flamepy.core.client import open_session as _open_core_session
from flamepy.core.types import FlameError, FlameErrorCode, ResourceRequirement

logger = logging.getLogger(__name__)

_FLMEXEC_APP = "flmexec"
_SUPPORTED_LANGUAGES = frozenset({"python", "shell"})


@dataclass(frozen=True)
class _SessionOptions:
    """Create-time specification for a Session. Frozen after construction."""

    language: str
    runtime: Optional[str] = None
    min_instances: int = 0
    max_instances: Optional[int] = None
    resreq: Optional[ResourceRequirement] = None


@dataclass
class SessionOutput:
    """Stdout from a session script."""

    data: bytes

    def text(self, encoding: str = "utf-8") -> str:
        return self.data.decode(encoding)


def _normalize_language(language: str) -> str:
    if not isinstance(language, str) or not language.strip():
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "language must be 'python' or 'shell'")
    normalized = language.strip().lower()
    if normalized not in _SUPPORTED_LANGUAGES:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, f"language must be 'python' or 'shell', got {language!r}")
    return normalized


def _copy_resreq(resreq: Optional[ResourceRequirement]) -> Optional[ResourceRequirement]:
    if resreq is None:
        return None
    return ResourceRequirement(cpu=resreq.cpu, memory=resreq.memory, gpu=resreq.gpu)


def _normalize_options(options: _SessionOptions) -> _SessionOptions:
    if not isinstance(options, _SessionOptions):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "invalid agent session options")
    return _SessionOptions(
        language=_normalize_language(options.language),
        runtime=options.runtime,
        min_instances=options.min_instances,
        max_instances=options.max_instances,
        resreq=_copy_resreq(options.resreq),
    )


def _encode_options(options: _SessionOptions) -> bytes:
    resreq = None
    if options.resreq is not None:
        resreq = {
            "cpu": options.resreq.cpu,
            "memory": options.resreq.memory,
            "gpu": options.resreq.gpu,
        }
    return json.dumps(
        {
            "language": options.language,
            "runtime": options.runtime,
            "min_instances": options.min_instances,
            "max_instances": options.max_instances,
            "resreq": resreq,
        }
    ).encode("utf-8")


def _decode_options(raw: Optional[bytes]) -> _SessionOptions:
    if not raw:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "agent session options are missing from common_data")
    try:
        payload = json.loads(raw.decode("utf-8"))
        resreq_data = payload.get("resreq")
        resreq = None
        if resreq_data is not None:
            resreq = ResourceRequirement(
                cpu=resreq_data.get("cpu", 0),
                memory=resreq_data.get("memory", 0),
                gpu=resreq_data.get("gpu", 0),
            )
        return _normalize_options(
            _SessionOptions(
                language=payload["language"],
                runtime=payload.get("runtime"),
                min_instances=payload.get("min_instances", 0),
                max_instances=payload.get("max_instances"),
                resreq=resreq,
            )
        )
    except FlameError:
        raise
    except Exception as exc:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "session common_data does not contain valid agent session options") from exc


def _encode_script(language: str, runtime: Optional[str], code: str, input_data: Optional[bytes]) -> bytes:
    if input_data is not None and not isinstance(input_data, bytes):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "input must be bytes or None")
    payload = {
        "language": language,
        "code": code,
        "input": None if input_data is None else list(input_data),
    }
    if runtime is not None:
        payload["runtime"] = runtime
    return json.dumps(payload).encode("utf-8")


def _decode_output(raw: Optional[bytes]) -> SessionOutput:
    if raw is None:
        return SessionOutput(data=b"")
    try:
        payload = json.loads(raw.decode("utf-8"))
        return SessionOutput(data=bytes(payload["data"]))
    except Exception as exc:
        raise FlameError(FlameErrorCode.INTERNAL, "response is not valid flmexec output JSON") from exc


class Session:
    """Domain facade for running remote Python or shell scripts."""

    def __init__(self, session, options: _SessionOptions):
        self._session = session
        self._options = options
        self._closed = False

    @property
    def attr(self) -> _SessionOptions:
        return self._options

    @property
    def id(self) -> str:
        if self._session is None:
            raise FlameError(FlameErrorCode.INVALID_STATE, "session is closed")
        return self._session.id

    def run_code(self, code: str, input: Optional[bytes] = None) -> SessionOutput:
        self._ensure_open()
        payload = _encode_script(self._options.language, self._options.runtime, code, input)
        return _decode_output(self._session.invoke(payload))

    def submit_code(self, code: str, input: Optional[bytes] = None) -> Future:
        self._ensure_open()
        payload = _encode_script(self._options.language, self._options.runtime, code, input)
        raw_future = self._session.run(payload)
        mapped: Future = Future()

        def complete(done: Future) -> None:
            try:
                mapped.set_result(_decode_output(done.result()))
            except Exception as exc:
                mapped.set_exception(exc)

        if isinstance(raw_future, _LazyTaskFuture):
            raw_future._add_internal_done_callback(complete)
        else:
            raw_future.add_done_callback(complete)
        return mapped

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        session = self._session
        self._session = None
        if session is not None:
            session.close()

    def _ensure_open(self) -> None:
        if self._closed or self._session is None:
            raise FlameError(FlameErrorCode.INVALID_STATE, "session is closed")

    def __enter__(self) -> "Session":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()


def open_session(
    *,
    ssn_id: Optional[str] = None,
    language: str = "python",
    runtime: Optional[str] = None,
    min_instances: int = 0,
    max_instances: Optional[int] = None,
    resreq: Optional[ResourceRequirement] = None,
) -> Session:
    """Create an agent session, or reopen one when ``ssn_id`` is provided."""
    if ssn_id is None:
        normalized_options = _normalize_options(
            _SessionOptions(
                language=language,
                runtime=runtime,
                min_instances=min_instances,
                max_instances=max_instances,
                resreq=resreq,
            )
        )
        logger.debug(
            "Creating agent session language=%s runtime=%s",
            normalized_options.language,
            normalized_options.runtime,
        )
        core_session = _create_core_session(
            _FLMEXEC_APP,
            common_data=_encode_options(normalized_options),
            min_instances=normalized_options.min_instances,
            max_instances=normalized_options.max_instances,
            resreq=normalized_options.resreq,
        )
    else:
        if not isinstance(ssn_id, str) or not ssn_id:
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "ssn_id must be a non-empty string")
        core_session = _open_core_session(ssn_id)
        normalized_options = None

    if core_session.application != _FLMEXEC_APP:
        raise FlameError(
            FlameErrorCode.INVALID_ARGUMENT,
            f"session {core_session.id!r} is not an agent session",
        )
    if normalized_options is None:
        normalized_options = _decode_options(core_session.common_data())
    return Session(core_session, normalized_options)
