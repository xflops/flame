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

from flamepy.core.client import create_session, open_session
from flamepy.core.types import FlameError, FlameErrorCode, ResourceRequirement

logger = logging.getLogger(__name__)

_FLMEXEC_APP = "flmexec"
_SUPPORTED_LANGUAGES = frozenset({"python", "shell"})


@dataclass(frozen=True)
class SandboxAttr:
    """Create-time specification for a Sandbox. Frozen after construction."""

    language: str
    runtime: Optional[str] = None
    min_instances: int = 0
    max_instances: Optional[int] = None
    resreq: Optional[ResourceRequirement] = None


@dataclass
class SandboxOutput:
    """Stdout from a sandbox script."""

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


def _normalize_attr(attr: SandboxAttr) -> SandboxAttr:
    if not isinstance(attr, SandboxAttr):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "attr must be a SandboxAttr")
    return SandboxAttr(
        language=_normalize_language(attr.language),
        runtime=attr.runtime,
        min_instances=attr.min_instances,
        max_instances=attr.max_instances,
        resreq=_copy_resreq(attr.resreq),
    )


def _encode_attr(attr: SandboxAttr) -> bytes:
    resreq = None
    if attr.resreq is not None:
        resreq = {
            "cpu": attr.resreq.cpu,
            "memory": attr.resreq.memory,
            "gpu": attr.resreq.gpu,
        }
    return json.dumps(
        {
            "language": attr.language,
            "runtime": attr.runtime,
            "min_instances": attr.min_instances,
            "max_instances": attr.max_instances,
            "resreq": resreq,
        }
    ).encode("utf-8")


def _decode_attr(raw: Optional[bytes]) -> SandboxAttr:
    if not raw:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "sandbox attr is missing from session common_data")
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
        return _normalize_attr(
            SandboxAttr(
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
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "session common_data is not a Sandbox attr") from exc


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


def _decode_output(raw: Optional[bytes]) -> SandboxOutput:
    if raw is None:
        return SandboxOutput(data=b"")
    try:
        payload = json.loads(raw.decode("utf-8"))
        return SandboxOutput(data=bytes(payload["data"]))
    except Exception as exc:
        raise FlameError(FlameErrorCode.INTERNAL, "response is not valid flmexec output JSON") from exc


class Sandbox:
    """Domain facade for running remote Python or shell scripts."""

    def __init__(self, session, attr: SandboxAttr):
        self._session = session
        self._attr = attr
        self._closed = False

    @classmethod
    def create(cls, attr: SandboxAttr) -> "Sandbox":
        attr = _normalize_attr(attr)
        logger.debug("Creating sandbox language=%s runtime=%s", attr.language, attr.runtime)
        session = create_session(
            _FLMEXEC_APP,
            common_data=_encode_attr(attr),
            min_instances=attr.min_instances,
            max_instances=attr.max_instances,
            resreq=attr.resreq,
        )
        return cls(session, attr)

    @classmethod
    def open(cls, sandbox_id: str) -> "Sandbox":
        session = open_session(sandbox_id)
        if session.application != _FLMEXEC_APP:
            raise FlameError(
                FlameErrorCode.INVALID_ARGUMENT,
                f"sandbox {sandbox_id!r} is not a script sandbox",
            )
        attr = _decode_attr(session.common_data())
        return cls(session, attr)

    @property
    def attr(self) -> SandboxAttr:
        return self._attr

    @property
    def sandbox_id(self) -> str:
        if self._session is None:
            raise FlameError(FlameErrorCode.INVALID_STATE, "sandbox is closed")
        return self._session.id

    def run_code(self, code: str, input: Optional[bytes] = None) -> SandboxOutput:
        self._ensure_open()
        payload = _encode_script(self._attr.language, self._attr.runtime, code, input)
        return _decode_output(self._session.invoke(payload))

    def submit_code(self, code: str, input: Optional[bytes] = None) -> Future:
        self._ensure_open()
        payload = _encode_script(self._attr.language, self._attr.runtime, code, input)
        raw_future = self._session.run(payload)
        mapped: Future = Future()

        def complete(done: Future) -> None:
            try:
                mapped.set_result(_decode_output(done.result()))
            except Exception as exc:
                mapped.set_exception(exc)

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
            raise FlameError(FlameErrorCode.INVALID_STATE, "sandbox is closed")

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()
