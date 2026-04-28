"""
Helper functions and classes for flmrun e2e tests.
These are defined in a separate module so they can be properly pickled.
"""

import json
from dataclasses import asdict
from typing import Optional

import cloudpickle
from flamepy import ObjectRef, get_object, put_object
from flamepy.runner import Runner, RunnerContext, RunnerRequest, SessionContext
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

    def __init__(self):
        self.count = 0

    def increment(self) -> int:
        self.count += 1
        return self.count

    def get_count(self) -> int:
        return self.count

    def add(self, value: int) -> int:
        self.count += value
        return self.count


def serialize_runner_context(runner_context: RunnerContext, app_name: str) -> bytes:
    """
    Serialize RunnerContext to bytes for core API.

    Uses cloudpickle serialization, then puts in cache to get ObjectRef, then encodes to bytes.

    Args:
        runner_context: RunnerContext object to serialize
        app_name: Application name for generating session ID

    Returns:
        bytes representation of ObjectRef
    """
    # Serialize the context using cloudpickle
    serialized_ctx = cloudpickle.dumps(runner_context, protocol=cloudpickle.DEFAULT_PROTOCOL)
    # Generate key prefix in <app>/<session> format for caching
    key_prefix = f"{app_name}/{short_name(app_name)}"
    # Put in cache to get ObjectRef
    object_ref = put_object(key_prefix, serialized_ctx)
    # Encode ObjectRef to bytes for core API
    return object_ref.encode()


def serialize_runner_request(request: RunnerRequest) -> bytes:
    """
    Serialize RunnerRequest to bytes for core API.

    Uses cloudpickle serialization.

    Args:
        request: RunnerRequest object to serialize

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
    """Service that recursively calls itself via Runner.service().

    This class demonstrates recursive task submission within the same session
    using the open_session API.
    """

    def __init__(self, session_id: str, app_name: str):
        """Initialize with session ID and app name for recursive calls.

        Args:
            session_id: The shared session ID for recursive calls.
            app_name: The shared application name for Runner.
        """
        self._session_context = SessionContext(
            session_id=session_id,
            application_name=app_name,
        )

    def compute_recursive(self, depth: int) -> int:
        """Compute recursively by creating new Runner and service instances.

        At depth 0, returns 1.
        At depth > 0, creates a new Runner (reusing existing app) and service
        with the same session, calls compute_recursive(depth - 1), then multiplies by 2.
        """
        import logging

        logger = logging.getLogger(__name__)

        logger.info(f"[RecursiveService] compute_recursive called with depth={depth}")
        logger.info(f"[RecursiveService] session_context: session_id={self._session_context.session_id}, app_name={self._session_context.application_name}")

        if depth <= 0:
            logger.info("[RecursiveService] Base case reached, returning 1")
            return 1

        try:
            logger.info(f"[RecursiveService] Creating inner Runner for app: {self._session_context.application_name}")

            # Create a new Runner to reuse existing app (fail_if_exists=False by default)
            # The outer Runner manages the lifecycle (registration/unregistration)
            with Runner(self._session_context.application_name) as inner_runner:
                logger.info(f"[RecursiveService] Inner Runner created, _app_registered={inner_runner._app_registered}")

                # Create service using Runner.service() with self
                # This reuses the existing session via _session_context
                # Use autoscale=True to allow multiple executors for recursive calls
                logger.info("[RecursiveService] Creating inner service with self")
                inner_service = inner_runner.service(self, autoscale=True)
                logger.info(f"[RecursiveService] Inner service created, session_id={inner_service._session.id}")

                # Call the inner service recursively
                logger.info(f"[RecursiveService] Calling compute_recursive({depth - 1}) on inner service")
                result = inner_service.compute_recursive(depth - 1)
                logger.info("[RecursiveService] Got result future, calling get()")
                inner_value = result.get()
                logger.info(f"[RecursiveService] Inner value = {inner_value}")

                final_result = inner_value * 2
                logger.info(f"[RecursiveService] Returning {final_result}")
                return final_result

        except Exception as e:
            logger.error(f"[RecursiveService] Exception in compute_recursive(depth={depth}): {type(e).__name__}: {e}", exc_info=True)
            raise
