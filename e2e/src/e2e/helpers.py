"""
Helper functions and classes for App end-to-end tests.
These are defined in a separate module so they can be properly pickled.
"""

import json
import os
import socket
import time
from dataclasses import asdict
from pathlib import Path
from typing import Optional

import cloudpickle
import flamepy.app as app
from flamepy import ObjectRef, get_object, put_object
from flamepy.util import short_name

from e2e.api import (
    ApplicationContextInfo,
    SessionContextInfo,
    TaskContextInfo,
    TestContext,
    TestRequest,
    TestResponse,
)


def sum_func(a: int, b: int) -> int:
    """Sum two integers."""
    return a + b


class DataAwareService:
    """Small App service used to exercise attribute publication and affinity."""

    def __init__(self) -> None:
        # Keep identity independent of the execution-object lifecycle so this
        # helper also covers function and supplied-object services.
        self.instance_key = f"e2e:data-aware:{socket.gethostname()}:{os.getpid()}".encode()
        endpoint = Path(os.environ["FLAME_INSTANCE_ENDPOINT"])
        self.executor_id = endpoint.parent.name if endpoint.name == "instance.sock" else endpoint.stem

    def run(self, value: str, delay: float = 0) -> tuple[str, bytes, str]:
        app.publish_attributes({self.instance_key})
        if delay:
            time.sleep(delay)
        return value, self.instance_key, self.executor_id


def multiply_func(a: int, b: int) -> int:
    """Multiply two integers."""
    return a * b


def greet_func(name: str, greeting: str = "Hello") -> str:
    """Greet someone."""
    return f"{greeting}, {name}!"


def get_message_func() -> str:
    """Get a message."""
    return "Hello from flmrun!"


def return_dict_func(key: str, value: int) -> dict:
    """Return a dictionary."""
    return {key: value}


def return_list_func(n: int) -> list:
    """Return a list."""
    return list(range(n))


def return_tuple_func(a: int, b: str) -> tuple:
    """Return a tuple."""
    return (a, b)


def square_func(x: int) -> int:
    """Square a number."""
    return x * x


def fuzzy_app_echo_case(input_value: str, output_value: str, common_data: str, sleep_ms: int = 0) -> dict:
    """Return a fuzzed App case after optionally simulating service work."""
    if sleep_ms > 0:
        time.sleep(sleep_ms / 1000.0)
    return {
        "common_data": common_data,
        "input": input_value,
        "output": output_value,
    }


class Calculator:
    """Simple calculator class."""

    def add(self, a: int, b: int) -> int:
        return a + b

    def multiply(self, a: int, b: int) -> int:
        return a * b

    def subtract(self, a: int, b: int) -> int:
        return a - b


class Counter:
    """Stateful counter class."""

    def __init__(self, count: int = 0):
        self.count = count

    def increment(self) -> int:
        self.count += 1
        return self.count

    def get_count(self) -> int:
        return self.count

    def add(self, value: int) -> int:
        self.count += value
        return self.count


def serialize_service_context(service_context: app.ServiceContext, app_name: str) -> bytes:
    """
    Serialize app.ServiceContext to bytes for core API.

    Uses cloudpickle serialization, then puts in cache to get ObjectRef, then encodes to bytes.

    Args:
        service_context: app.ServiceContext object to serialize
        app_name: Application name for generating session ID

    Returns:
        bytes representation of ObjectRef
    """
    # Serialize the context using cloudpickle
    serialized_ctx = cloudpickle.dumps(service_context, protocol=cloudpickle.DEFAULT_PROTOCOL)
    # Generate key prefix in <app>/<session> format for caching
    key_prefix = f"{app_name}/{short_name(app_name)}"
    # Put in cache to get ObjectRef
    object_ref = put_object(key_prefix, serialized_ctx)
    # Encode ObjectRef to bytes for core API
    return object_ref.encode()


def serialize_app_service_request(request: app.ServiceRequest) -> bytes:
    """
    Serialize app.ServiceRequest to bytes for core API.

    Uses cloudpickle serialization.

    Args:
        request: app.ServiceRequest object to serialize

    Returns:
        bytes representation of the request
    """
    return cloudpickle.dumps(request, protocol=cloudpickle.DEFAULT_PROTOCOL)


def serialize_common_data(common_data: Optional[TestContext], app_name: str) -> Optional[bytes]:
    """
    Serialize common data to bytes for core API.

    Uses JSON serialization, then puts in cache to get ObjectRef, then encodes to bytes.

    Args:
        common_data: TestContext object to serialize, or None
        app_name: Application name for generating session ID

    Returns:
        bytes representation of ObjectRef, or None if common_data is None
    """
    if common_data is None:
        return None

    # Serialize with JSON
    serialized_ctx = json.dumps(asdict(common_data)).encode("utf-8")
    # Put in cache to get ObjectRef
    key_prefix = f"{app_name}/{short_name(app_name)}"
    object_ref = put_object(key_prefix, serialized_ctx)
    # Encode ObjectRef to bytes for core API
    return object_ref.encode()


def deserialize_common_data(
    common_data_bytes: Optional[bytes],
) -> Optional[TestContext]:
    """
    Deserialize common data from bytes.

    Decodes bytes to ObjectRef, gets from cache, then deserializes from JSON.

    Args:
        common_data_bytes: bytes representation of ObjectRef, or None

    Returns:
        TestContext object, or None if common_data_bytes is None
    """
    if common_data_bytes is None:
        return None

    # Decode bytes to ObjectRef
    object_ref = ObjectRef.decode(common_data_bytes)
    # Get from cache (returns JSON bytes)
    serialized_ctx = get_object(object_ref)
    # Deserialize from JSON
    ctx_dict = json.loads(serialized_ctx.decode("utf-8"))
    return TestContext(**ctx_dict)


def serialize_request(request: TestRequest) -> bytes:
    """
    Serialize a TestRequest to bytes using JSON.

    Args:
        request: TestRequest object

    Returns:
        bytes representation of the request
    """
    request_dict = asdict(request)
    return json.dumps(request_dict).encode("utf-8")


def deserialize_request(request_bytes: bytes) -> TestRequest:
    """
    Deserialize bytes to TestRequest using JSON.

    Args:
        request_bytes: bytes representation of the request

    Returns:
        TestRequest object
    """
    request_dict = json.loads(request_bytes.decode("utf-8"))
    return TestRequest(**request_dict)


def serialize_response(response: TestResponse) -> bytes:
    """
    Serialize a TestResponse to bytes using JSON.

    Args:
        response: TestResponse object

    Returns:
        bytes representation of the response
    """
    response_dict = asdict(response)
    return json.dumps(response_dict).encode("utf-8")


def deserialize_response(response_bytes: bytes) -> TestResponse:
    """
    Deserialize bytes to TestResponse using JSON.

    Args:
        response_bytes: bytes representation of the response

    Returns:
        TestResponse object
    """
    response_dict = json.loads(response_bytes.decode("utf-8"))

    # Convert nested dictionaries to proper dataclass instances
    if "task_context" in response_dict and response_dict["task_context"] is not None:
        response_dict["task_context"] = TaskContextInfo(**response_dict["task_context"])

    if "session_context" in response_dict and response_dict["session_context"] is not None:
        session_ctx_dict = response_dict["session_context"]
        # Convert nested application context if present
        if "application" in session_ctx_dict and session_ctx_dict["application"] is not None:
            session_ctx_dict["application"] = ApplicationContextInfo(**session_ctx_dict["application"])
        response_dict["session_context"] = SessionContextInfo(**session_ctx_dict)

    if "application_context" in response_dict and response_dict["application_context"] is not None:
        response_dict["application_context"] = ApplicationContextInfo(**response_dict["application_context"])

    return TestResponse(**response_dict)


def invoke_task(session, request: TestRequest) -> TestResponse:
    """
    Helper function to invoke a task and deserialize the response.

    Args:
        session: Session object
        request: TestRequest object

    Returns:
        TestResponse object
    """
    request_bytes = serialize_request(request)
    response_bytes = session.invoke(request_bytes)
    return deserialize_response(response_bytes)


class RecursiveService:
    """Service that recursively calls itself via App.service().

    This class demonstrates recursive task submission within the same session
    using the open_session API.
    """

    def compute_recursive(self, depth: int, include_session_ids: bool = False):
        """Compute recursively by declaring another proxy for this session.

        At depth 0, returns 1.
        At depth > 0, declares a service with the existing session context,
        calls compute_recursive(depth - 1), then multiplies by 2.
        When include_session_ids is true, also returns the session ID observed
        at every recursion level so callers can verify session reuse.
        """
        import logging

        logger = logging.getLogger(__name__)

        logger.info(f"[RecursiveService] compute_recursive called with depth={depth}")
        session_context = app.session_context()
        logger.info(f"[RecursiveService] session_context: session_id={session_context.session_id}, app_name={session_context.application.name}")

        if depth <= 0:
            logger.info("[RecursiveService] Base case reached, returning 1")
            if include_session_ids:
                return 1, [session_context.session_id]
            return 1

        try:
            # A recursive declaration reuses the core SessionContext and does
            # not own the parent application or session lifecycle. It therefore
            # needs neither app.init() nor app.destroy().
            logger.info("[RecursiveService] Creating inner service with class")

            @app.service()
            class InnerRecursiveService(type(self)):
                pass

            inner_service = InnerRecursiveService()
            logger.info(f"[RecursiveService] Inner service created, session_id={inner_service._session.id}")

            logger.info(f"[RecursiveService] Calling compute_recursive({depth - 1}) on inner service")
            result = inner_service.compute_recursive(depth - 1, include_session_ids)
            logger.info("[RecursiveService] Got result future, calling get()")
            inner_value = result.get()
            logger.info(f"[RecursiveService] Inner value = {inner_value}")

            if include_session_ids:
                value, session_ids = inner_value
                final_result = value * 2
                return final_result, [session_context.session_id, *session_ids]

            final_result = inner_value * 2
            logger.info(f"[RecursiveService] Returning {final_result}")
            return final_result

        except Exception as e:
            logger.error(f"[RecursiveService] Exception in compute_recursive(depth={depth}): {type(e).__name__}: {e}", exc_info=True)
            raise
