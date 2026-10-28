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
from typing import Dict, Optional

from flamepy.agent.sandbox import CommandResult, _decode_result, _encode_request
from flamepy.core.client import create_session as _create_core_session
from flamepy.core.client import open_session as _open_core_session
from flamepy.core.types import FlameError, FlameErrorCode, ResourceRequirement, SessionAttributes

logger = logging.getLogger(__name__)

_FLMBOX_APP = "flmbox"


@dataclass(frozen=True)
class _SessionOptions:
    """Create-time specification for a Session. Frozen after construction."""

    image: Optional[str] = None
    min_instances: int = 0
    max_instances: Optional[int] = None
    resreq: Optional[ResourceRequirement] = None


def _normalize_image(image: Optional[str]) -> Optional[str]:
    if image is not None and (not isinstance(image, str) or not image.strip()):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "image must be a non-empty string or None")
    return image


def _copy_resreq(resreq: Optional[ResourceRequirement]) -> Optional[ResourceRequirement]:
    if resreq is None:
        return None
    return ResourceRequirement(cpu=resreq.cpu, memory=resreq.memory, gpu=resreq.gpu)


def _normalize_options(options: _SessionOptions) -> _SessionOptions:
    if not isinstance(options, _SessionOptions):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "invalid agent session options")
    return _SessionOptions(
        image=_normalize_image(options.image),
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
            "image": options.image,
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
                image=payload.get("image"),
                min_instances=payload.get("min_instances", 0),
                max_instances=payload.get("max_instances"),
                resreq=resreq,
            )
        )
    except FlameError:
        raise
    except Exception as exc:
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "session common_data does not contain valid agent session options") from exc


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

    def run_code(
        self,
        code: str,
        *,
        language: str = "shell",
        cwd: Optional[str] = None,
        envs: Optional[Dict[str, str]] = None,
        input: Optional[bytes] = None,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        """Run a ``shell`` (default) or ``python`` script with optional stdin bytes. A nonzero exit is a normal result."""
        self._ensure_open()
        payload = _encode_request(code, language, cwd, envs, input, timeout)
        return _decode_result(self._session.run(payload))

    def submit_code(
        self,
        code: str,
        *,
        language: str = "shell",
        cwd: Optional[str] = None,
        envs: Optional[Dict[str, str]] = None,
        input: Optional[bytes] = None,
        timeout: Optional[float] = None,
    ) -> "Future[CommandResult]":
        self._ensure_open()
        payload = _encode_request(code, language, cwd, envs, input, timeout)
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
    image: Optional[str] = None,
    min_instances: int = 0,
    max_instances: Optional[int] = None,
    resreq: Optional[ResourceRequirement] = None,
) -> Session:
    """Create an agent session, or open or create the one named ``ssn_id``.

    With ``ssn_id`` an existing session is reopened and a missing one is created with that id. The
    creation options only apply when the session is created; an existing session keeps its own.
    ``image`` is the sandbox container image; the ``flmbox`` service default applies when it is ``None``.
    """
    if ssn_id is not None and (not isinstance(ssn_id, str) or not ssn_id):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "ssn_id must be a non-empty string")
    normalized_options = _normalize_options(
        _SessionOptions(
            image=image,
            min_instances=min_instances,
            max_instances=max_instances,
            resreq=resreq,
        )
    )
    common_data = _encode_options(normalized_options)
    if ssn_id is None:
        logger.debug("Creating agent session image=%s", normalized_options.image)
        core_session = _create_core_session(
            _FLMBOX_APP,
            common_data=common_data,
            min_instances=normalized_options.min_instances,
            max_instances=normalized_options.max_instances,
            resreq=normalized_options.resreq,
        )
    else:
        core_session = _open_core_session(
            ssn_id,
            SessionAttributes(
                application=_FLMBOX_APP,
                id=ssn_id,
                common_data=common_data,
                min_instances=normalized_options.min_instances,
                max_instances=normalized_options.max_instances,
                resreq=normalized_options.resreq,
            ),
        )

    if core_session.application != _FLMBOX_APP:
        raise FlameError(
            FlameErrorCode.INVALID_ARGUMENT,
            f"session {core_session.id!r} is not an agent session",
        )
    if ssn_id is not None:
        # The session may have existed already, with its own options.
        normalized_options = _decode_options(core_session.common_data())
    return Session(core_session, normalized_options)
