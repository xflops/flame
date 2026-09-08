"""
Copyright 2025 The Flame Authors.
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

import os
import tempfile
from pathlib import Path

import flamepy
import pytest
from flamepy import TaskOptions, runner
from flamepy.runner import SessionContext

from e2e.helpers import (
    Calculator,
    Counter,
    DataAwareService,
    RecursiveService,
    greet_func,
    sum_func,
)


@pytest.fixture(scope="module")
def check_package_config():
    """Check that storage configuration is available (via package.storage or cache.endpoint)."""
    ctx = flamepy.FlameContext()
    # Storage can come from either package.storage or cache.endpoint
    has_package_storage = ctx.package is not None and getattr(ctx.package, "storage", None) is not None
    has_cache_endpoint = ctx.cache is not None
    if not has_package_storage and not has_cache_endpoint:
        pytest.skip("Storage configuration not set in flame.yaml. Please add 'cache.endpoint' or 'package.storage' section.")
    yield ctx.package


@pytest.fixture(scope="module")
def check_flmrun_app():
    """Check that flmrun application is registered."""
    try:
        flamepy.get_application("flmrun")
    except Exception:
        pytest.skip("flmrun application not found. Please ensure it's registered.")


def test_runner_context_manager(check_package_config, check_flmrun_app):
    """Test Case 1: Test Runner as a context manager."""
    # Use Runner as a context manager
    with runner.Runner("test-runner-cm"):
        # Verify that the application is registered
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-runner-cm" in app_names, f"test-runner-cm not found in applications: {app_names}"

    # After exiting context, application should be unregistered
    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-runner-cm" not in app_names, f"test-runner-cm should be unregistered but found in: {app_names}"


def test_runner_with_function(check_package_config, check_flmrun_app):
    """Test Case 2: Test Runner with a simple function."""
    with runner.Runner("test-runner-func") as rr:
        # Create a service with a function
        sum_service = rr.service(sum_func)

        # Call the function remotely
        result = sum_service(1, 3)

        # Verify result is an ObjectFuture
        assert isinstance(result, runner.ObjectFuture), f"Expected ObjectFuture, got {type(result)}"

        # Get the actual result
        value = result.get()
        assert value == 4, f"Expected 4, got {value}"


def test_runner_with_class(check_package_config, check_flmrun_app):
    """Test Case 3: Test Runner with a class (auto-instantiation)."""
    with runner.Runner("test-runner-class") as rr:
        # Create a service with a class (should auto-instantiate).
        calc_s = rr.service(Calculator)

        # Class services are not stateful, so use a stateless method here.
        res_r = calc_s.multiply(2, 3)

        # Get the result
        value = res_r.get()
        assert value == 6, f"Expected 6, got {value}"


def test_runner_data_aware_scheduling(check_package_config, check_flmrun_app):
    """Runner carries a service publication into a later affinity task."""
    with runner.Runner("test-runner-das") as rr:
        service = rr.service(DataAwareService, warmup=1)
        _, affinity_key = service.run("warmup").get()
        affinity = service.run(
            "affinity-match",
            option=TaskOptions(affinity={affinity_key}),
        )

        value, _ = affinity.get()
        assert value == "affinity-match"


def test_runner_with_instance(check_package_config, check_flmrun_app):
    """Test Case 4: Test Runner with a class instance."""
    with runner.Runner("test-runner-instance") as rr:
        # Create a Counter instance with initial value
        counter = Counter()
        # Set initial count to 10 by adding 10
        counter.add(10)

        # Instance services are stateful and fixed by default.
        cnt_os = rr.service(counter)

        # Apply state changes sequentially so the expected total is deterministic.
        cnt_os.increment().wait()
        cnt_os.add(3).wait()
        res_r = cnt_os.get_count()

        # Get the result
        value = res_r.get()
        assert value == 14, f"Expected 14, got {value}"


def test_runner_with_objectfuture_args(check_package_config, check_flmrun_app):
    """Test Case 5: Test Runner with ObjectFuture as arguments."""
    with runner.Runner("test-runner-objfuture") as rr:
        # Create a Counter instance with initial value
        counter = Counter()
        counter.add(10)

        # Instance services are stateful and fixed by default.
        cnt_os = rr.service(counter)

        # Apply state changes sequentially so ObjectFuture chaining starts from
        # a deterministic counter value.
        cnt_os.increment().wait()
        cnt_os.add(3).wait()
        res_r = cnt_os.get_count()

        # Use ObjectFuture as argument
        cnt_os.add(res_r).wait()
        res_r2 = cnt_os.get_count()

        # Get the result
        value = res_r2.get()
        assert value == 28, f"Expected 28, got {value}"


def test_runner_multiple_services(check_package_config, check_flmrun_app):
    """Test Case 6: Test Runner with multiple services."""
    with runner.Runner("test-runner-multi") as rr:
        # Create multiple services
        sum_service = rr.service(sum_func)
        calc_service = rr.service(Calculator())

        # Call methods on different services
        result1 = sum_service(5, 3)
        result2 = calc_service.multiply(4, 7)

        # Get results
        value1, value2 = rr.get([result1, result2])

        assert value1 == 8, f"Expected 8, got {value1}"
        assert value2 == 28, f"Expected 28, got {value2}"


def test_runner_with_kwargs(check_package_config, check_flmrun_app):
    """Test Case 7: Test Runner with keyword arguments."""
    with runner.Runner("test-runner-kwargs") as rr:
        # Create a service with a function that accepts kwargs
        greet_service = rr.service(greet_func)

        # Call with keyword arguments
        result = greet_service(name="World", greeting="Hi")

        # Call with partial kwargs (uses default)
        result2 = greet_service(name="Python")

        value1, value2 = rr.get([result, result2])
        assert value1 == "Hi, World!", f"Expected 'Hi, World!', got {value1}"
        assert value2 == "Hello, Python!", f"Expected 'Hello, Python!', got {value2}"


def test_runner_package_excludes(check_package_config, check_flmrun_app):
    """Test Case 8: Test that package excludes work properly."""
    # Create a temporary directory with test files
    with tempfile.TemporaryDirectory() as tmpdir:
        # Save current directory
        original_dir = os.getcwd()

        try:
            # Change to temp directory
            os.chdir(tmpdir)

            # Create some test files
            Path("main.py").write_text("print('hello')")
            Path("test.log").write_text("log content")
            Path("data.pkl").write_text("pickle content")
            os.makedirs("__pycache__", exist_ok=True)
            Path("__pycache__/test.pyc").write_text("compiled")

            # Use Runner (should exclude .log, .pkl, __pycache__)
            with runner.Runner("test-runner-excludes"):
                # Just verify it works - the exclusion is tested by successful packaging
                pass

        finally:
            # Restore original directory
            os.chdir(original_dir)


def test_objectfuture_ref_method(check_package_config, check_flmrun_app):
    """Test Case 9: Test ObjectFuture.ref() method."""
    with runner.Runner("test-objectfuture-ref") as rr:
        sum_service = rr.service(sum_func)

        # Get an ObjectFuture
        result = sum_service(10, 20)

        # Get the ObjectRef
        obj_ref = result.ref()

        # Verify it's an ObjectRef
        assert isinstance(obj_ref, flamepy.core.ObjectRef), f"Expected ObjectRef, got {type(obj_ref)}"
        assert obj_ref.endpoint is not None, "ObjectRef endpoint should not be None"
        assert obj_ref.key is not None, "ObjectRef key should not be None"


def test_objectfuture_iterator(check_package_config, check_flmrun_app):
    """Test Case 10: Test ObjectFutureIterator."""
    with runner.Runner("test-objectfuture-iterator") as rr:
        sum_service = rr.service(sum_func)

        results = [
            sum_service(1, 2),
            sum_service(5, 7),
            sum_service(3, 4),
        ]

        values = []
        for result in rr.select(results):
            values.append(result.get())

        assert sorted(values) == [3, 7, 12]


def test_runner_service_close(check_package_config, check_flmrun_app):
    """Test Case 11: Test that RunnerService.close() works."""
    with runner.Runner("test-service-close") as rr:
        sum_service = rr.service(sum_func)

        # Use the service
        result = sum_service(1, 2)
        assert result.get() == 3

        # Close should be called automatically on context exit
        # This test just verifies no errors occur


def test_flame_package_dataclass():
    """Test Case 12: Test FlamePackage dataclass."""
    # Test with defaults
    pkg1 = flamepy.FlamePackage(storage="file:///tmp/test")
    assert pkg1.storage == "file:///tmp/test"
    assert ".venv" in pkg1.excludes
    assert "__pycache__" in pkg1.excludes
    assert "*.pyc" in pkg1.excludes

    # Test with custom excludes
    pkg2 = flamepy.FlamePackage(storage="file:///tmp/test", excludes=["*.log", "*.tmp"])
    assert pkg2.storage == "file:///tmp/test"
    assert pkg2.excludes == ["*.log", "*.tmp"]


def test_runner_error_no_storage_config():
    """Test Runner fails gracefully without storage config (no package.storage and no cache.endpoint)."""
    ctx = flamepy.FlameContext()
    has_package_storage = ctx.package is not None and getattr(ctx.package, "storage", None) is not None
    has_cache_endpoint = ctx.cache is not None
    if has_package_storage or has_cache_endpoint:
        pytest.skip("Storage config is available (package.storage or cache.endpoint), cannot test error case")

    with pytest.raises(flamepy.FlameError) as exc_info:
        with runner.Runner("test-no-config"):
            pass

    assert exc_info.value.code == flamepy.FlameErrorCode.INVALID_CONFIG


def test_runner_stateful_instance(check_package_config, check_flmrun_app):
    """Test Case 14: Test Runner with default stateful instance."""
    with runner.Runner("test-runner-stateful") as rr:
        # Create a Counter instance
        counter = Counter()

        # Instance services are stateful by default.
        cnt_service = rr.service(counter)

        # Call methods
        cnt_service.add(5).wait()
        cnt_service.increment().wait()
        result = cnt_service.get_count()

        # Get the result
        value = result.get()
        assert value == 6, f"Expected 6, got {value}"


def test_runner_stateless_function(check_package_config, check_flmrun_app):
    """Test Case 15: Test Runner with stateless function (default behavior)."""
    with runner.Runner("test-runner-stateless-func") as rr:
        # Create a service with a function (stateless by default)
        sum_service = rr.service(sum_func)

        # Call the function multiple times
        results = [sum_service(i, i + 1) for i in range(5)]
        values = rr.get(results)

        # Verify results
        expected = [1, 3, 5, 7, 9]
        assert values == expected, f"Expected {expected}, got {values}"


def test_runner_class_single_instance(check_package_config, check_flmrun_app):
    """Test Case 16: Test Runner with class and autoscale=False (single instance)."""
    with runner.Runner("test-runner-class-single") as rr:
        # Create a service with a class, single instance mode
        calc_service = rr.service(Calculator, autoscale=False)

        # Call methods
        result1 = calc_service.add(10, 5)
        result2 = calc_service.multiply(3, 4)

        values = rr.get([result1, result2])
        assert values == [15, 12], f"Expected [15, 12], got {values}"


def test_runner_error_object_autoscale(check_package_config, check_flmrun_app):
    """Test Case 17: Test that object instances cannot autoscale."""
    with runner.Runner("test-runner-object-autoscale-error") as rr:
        # Object instance services are always fixed.
        with pytest.raises(ValueError) as exc_info:
            rr.service(Counter(), autoscale=True)

        assert "always fixed" in str(exc_info.value)


def test_runner_defaults_function(check_package_config, check_flmrun_app):
    """Test Case 18: Test default parameters for function (stateful=False, autoscale=True)."""
    with runner.Runner("test-runner-defaults-func") as rr:
        # Create service with defaults (should be stateful=False, autoscale=True)
        sum_service = rr.service(sum_func)

        # Verify it works (defaults should be applied automatically)
        result = sum_service(100, 200)
        value = result.get()
        assert value == 300, f"Expected 300, got {value}"


def test_runner_defaults_class(check_package_config, check_flmrun_app):
    """Test Case 19: Test default parameters for class (stateful=False, autoscale=True)."""
    with runner.Runner("test-runner-defaults-class") as rr:
        # Create service with class using defaults (should be stateful=False, autoscale=True)
        calc_service = rr.service(Calculator)

        # Use a stateless method because class services cannot be stateful.
        result = calc_service.add(10, 1)

        value = result.get()
        assert value == 11, f"Expected 11, got {value}"


def test_runner_defaults_instance(check_package_config, check_flmrun_app):
    """Test Case 20: Test default parameters for instance (stateful=True, autoscale=False)."""
    with runner.Runner("test-runner-defaults-instance") as rr:
        counter = Counter()

        # Create service with instance using defaults (should be stateful=True, autoscale=False)
        counter_service = rr.service(counter)

        counter_service.add(5).wait()
        counter_service.increment().wait()
        result = counter_service.get_count()

        value = result.get()
        assert value == 6, f"Expected 6, got {value}"


def test_runner_auto_start(check_package_config, check_flmrun_app):
    """Test Case 21: Test Runner starts automatically in __init__."""
    rr = runner.Runner("test-runner-auto-start")
    try:
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-runner-auto-start" in app_names, f"test-runner-auto-start not found in applications: {app_names}"

        sum_service = rr.service(sum_func)
        result = sum_service(10, 20)
        value = result.get()
        assert value == 30, f"Expected 30, got {value}"
    finally:
        rr.close()

    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-runner-auto-start" not in app_names, f"test-runner-auto-start should be unregistered but found in: {app_names}"


def test_runner_explicit_close(check_package_config, check_flmrun_app):
    """Test Case 22: Test Runner with explicit close() call."""
    rr = runner.Runner("test-runner-explicit-close")

    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-runner-explicit-close" in app_names

    sum_service = rr.service(sum_func)
    result = sum_service(5, 7)
    value = result.get()
    assert value == 12, f"Expected 12, got {value}"

    rr.close()

    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-runner-explicit-close" not in app_names


def test_runner_fail_if_exists_true(check_package_config, check_flmrun_app):
    """Test Case 23: Test Runner with fail_if_exists=True raises error for existing app."""
    rr1 = runner.Runner("test-runner-exists-check")
    try:
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-runner-exists-check" in app_names

        with pytest.raises(flamepy.FlameError) as exc_info:
            runner.Runner("test-runner-exists-check", fail_if_exists=True)

        assert exc_info.value.code == flamepy.FlameErrorCode.ALREADY_EXISTS
    finally:
        rr1.close()


def test_runner_fail_if_exists_false(check_package_config, check_flmrun_app):
    """Test Case 24: Test Runner with fail_if_exists=False (default) skips registration."""
    rr1 = runner.Runner("test-runner-exists-skip")
    try:
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-runner-exists-skip" in app_names

        rr2 = runner.Runner("test-runner-exists-skip")

        sum_service = rr2.service(sum_func)
        result = sum_service(3, 4)
        value = result.get()
        assert value == 7, f"Expected 7, got {value}"

        rr2.close()

        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-runner-exists-skip" in app_names
    finally:
        rr1.close()


def test_runner_close_idempotent(check_package_config, check_flmrun_app):
    """Test Case 25: Test that calling close() multiple times is safe."""
    with runner.Runner("test-runner-close-idempotent") as rr:
        sum_service = rr.service(sum_func)
        result = sum_service(1, 1)
        assert result.get() == 2

    rr.close()
    rr.close()


# =============================================================================
# SessionContext Tests (RFE350)
# =============================================================================


def test_session_context_with_class(check_package_config, check_flmrun_app):
    """Test Case 26: Test SessionContext with a class."""

    # Define a class with custom session context
    class ServiceWithContext:
        _session_context = SessionContext(session_id="test-class-session-001", application_name="test-class-app")

        def compute(self, x: int) -> int:
            return x * 2

    with runner.Runner("test-session-ctx-class") as rr:
        service = rr.service(ServiceWithContext)

        # Verify the session ID matches
        assert service._session.id == "test-class-session-001", f"Expected session ID 'test-class-session-001', got '{service._session.id}'"

        # Verify the service works
        result = service.compute(21)
        value = result.get()
        assert value == 42, f"Expected 42, got {value}"


def test_session_context_with_instance(check_package_config, check_flmrun_app):
    """Test Case 27: Test SessionContext with an instance (object)."""
    # Create an instance and attach context
    counter = Counter()
    counter._session_context = SessionContext(session_id="test-instance-session-001", application_name="test-instance-app")

    with runner.Runner("test-session-ctx-instance") as rr:
        service = rr.service(counter)

        # Verify the session ID matches
        assert service._session.id == "test-instance-session-001", f"Expected session ID 'test-instance-session-001', got '{service._session.id}'"

        # Verify the service works
        service.add(10).wait()
        result = service.get_count()
        value = result.get()
        assert value == 10, f"Expected 10, got {value}"


def test_session_context_with_function(check_package_config, check_flmrun_app):
    """Test Case 28: Test SessionContext with a function."""

    # Create a function and attach context
    def my_sum(a: int, b: int) -> int:
        return a + b

    my_sum._session_context = SessionContext(session_id="test-func-session-001", application_name="test-func-app")

    with runner.Runner("test-session-ctx-func") as rr:
        service = rr.service(my_sum)

        # Verify the session ID matches
        assert service._session.id == "test-func-session-001", f"Expected session ID 'test-func-session-001', got '{service._session.id}'"

        # Verify the service works
        result = service(10, 20)
        value = result.get()
        assert value == 30, f"Expected 30, got {value}"


def test_session_context_no_session_id(check_package_config, check_flmrun_app):
    """Test Case 29: Test SessionContext with session_id=None (auto-generate)."""

    class ServiceWithPartialContext:
        _session_context = SessionContext(
            application_name="partial-ctx-app"
            # session_id is None, should auto-generate
        )

        def echo(self, msg: str) -> str:
            return msg

    with runner.Runner("test-session-ctx-partial") as rr:
        service = rr.service(ServiceWithPartialContext)

        # Session ID should be auto-generated (starts with app name prefix)
        assert service._session.id.startswith("test-session-ctx-partial"), f"Expected session ID to start with 'test-session-ctx-partial', got '{service._session.id}'"

        # Verify the service works
        result = service.echo("hello")
        value = result.get()
        assert value == "hello", f"Expected 'hello', got {value}"


def test_session_context_without_context(check_package_config, check_flmrun_app):
    """Test Case 30: Test that services without SessionContext still work (backward compatibility)."""

    # Standard class without _session_context
    class PlainService:
        def multiply(self, x: int, y: int) -> int:
            return x * y

    with runner.Runner("test-no-session-ctx") as rr:
        service = rr.service(PlainService)

        # Session ID should be auto-generated
        assert service._session.id.startswith("test-no-session-ctx"), f"Expected session ID to start with 'test-no-session-ctx', got '{service._session.id}'"

        # Verify the service works
        result = service.multiply(7, 8)
        value = result.get()
        assert value == 56, f"Expected 56, got {value}"


def test_session_context_invalid_type_ignored(check_package_config, check_flmrun_app):
    """Test Case 31: Test that invalid _session_context type is ignored with warning."""

    class ServiceWithInvalidContext:
        # Invalid type - should be ignored
        _session_context = {"session_id": "invalid"}

        def add(self, a: int, b: int) -> int:
            return a + b

    with runner.Runner("test-invalid-ctx-type") as rr:
        # Should not raise error, just ignore the invalid context
        service = rr.service(ServiceWithInvalidContext)

        # Session ID should be auto-generated since invalid context was ignored
        assert service._session.id.startswith("test-invalid-ctx-type"), f"Expected session ID to start with 'test-invalid-ctx-type', got '{service._session.id}'"

        # Verify the service works
        result = service.add(5, 3)
        value = result.get()
        assert value == 8, f"Expected 8, got {value}"


def test_session_context_validation_empty_string():
    """Test Case 32: Test SessionContext validation - empty session_id."""
    with pytest.raises(ValueError) as exc_info:
        SessionContext(session_id="")

    assert "cannot be empty" in str(exc_info.value)


def test_session_context_validation_too_long():
    """Test Case 33: Test SessionContext validation - session_id too long."""
    with pytest.raises(ValueError) as exc_info:
        SessionContext(session_id="x" * 129)

    assert "too long" in str(exc_info.value)


def test_session_context_validation_invalid_type():
    """Test Case 34: Test SessionContext validation - invalid session_id type."""
    with pytest.raises(ValueError) as exc_info:
        SessionContext(session_id=12345)  # type: ignore

    assert "must be a string" in str(exc_info.value)


def test_session_context_validation_invalid_app_name():
    """Test Case 35: Test SessionContext validation - invalid application_name type."""
    with pytest.raises(ValueError) as exc_info:
        SessionContext(application_name=12345)  # type: ignore

    assert "must be a string" in str(exc_info.value)


def test_session_context_valid_creation():
    """Test Case 36: Test SessionContext valid creation."""
    # Test with all fields
    ctx1 = SessionContext(session_id="valid-session-123", application_name="my-app")
    assert ctx1.session_id == "valid-session-123"
    assert ctx1.application_name == "my-app"

    # Test with only session_id
    ctx2 = SessionContext(session_id="only-session")
    assert ctx2.session_id == "only-session"
    assert ctx2.application_name is None

    # Test with only application_name
    ctx3 = SessionContext(application_name="only-app")
    assert ctx3.session_id is None
    assert ctx3.application_name == "only-app"

    # Test with no fields (all defaults)
    ctx4 = SessionContext()
    assert ctx4.session_id is None
    assert ctx4.application_name is None


def test_session_context_max_length():
    """Test Case 37: Test SessionContext with max length session_id (128 chars)."""
    max_session_id = "x" * 128
    ctx = SessionContext(session_id=max_session_id)
    assert ctx.session_id == max_session_id
    assert len(ctx.session_id) == 128


def test_session_context_dynamic_class(check_package_config, check_flmrun_app):
    """Test Case 38: Test SessionContext with dynamically created class."""

    def create_service_class(session_id: str):
        class DynamicService:
            _session_context = SessionContext(session_id=session_id)

            def get_id(self) -> str:
                return session_id

        return DynamicService

    service_class = create_service_class("dynamic-session-001")

    with runner.Runner("test-dynamic-ctx") as rr:
        service = rr.service(service_class)

        # Verify the session ID matches
        assert service._session.id == "dynamic-session-001", f"Expected session ID 'dynamic-session-001', got '{service._session.id}'"

        # Verify the service works
        result = service.get_id()
        value = result.get()
        assert value == "dynamic-session-001", f"Expected 'dynamic-session-001', got {value}"


# =============================================================================
# Recursive Runner Tests (open_session)
# =============================================================================


def test_runner_recursive_same_session(check_package_config, check_flmrun_app):
    """Test Case 39: Test recursive runner execution within the same session.

    This test verifies that a task can create another RunnerService using the same
    session ID, enabling recursive task submission within the same session.
    The open_session API allows this by returning the existing session instead of
    creating a new one.

    The outer Runner manages the lifecycle, while inner Runner instances with
    fail_if_exists=False reuse the existing application registration.
    """
    import logging
    import time
    import uuid

    logging.basicConfig(level=logging.INFO)
    logger = logging.getLogger(__name__)

    # Shared application name and session ID
    recursive_suffix = uuid.uuid4().hex[:8]
    shared_app_name = f"test-runner-recursive-{recursive_suffix}"
    shared_session_id = f"recursive-session-{recursive_suffix}"

    logger.info(f"[TEST] Starting recursive test: app={shared_app_name}, session={shared_session_id}")

    class RecursiveTestService(RecursiveService):
        _session_context = SessionContext(
            session_id=shared_session_id,
            application_name=shared_app_name,
        )

    with runner.Runner(shared_app_name) as rr:
        # Use autoscale=True to allow multiple executors for recursive calls
        # Without autoscale, a single executor would deadlock waiting for its own recursive task
        service = rr.service(RecursiveTestService, autoscale=True)
        logger.info(f"[TEST] Service created, session_id={service._session.id}")

        # Verify the session ID matches
        assert service._session.id == shared_session_id, f"Expected session ID '{shared_session_id}', got '{service._session.id}'"

        # Test with depth=0 (base case)
        logger.info("[TEST] Testing depth=0")
        start_time = time.time()
        result0 = service.compute_recursive(0)
        value0 = result0.get()
        logger.info(f"[TEST] depth=0 result={value0} ({time.time() - start_time:.2f}s)")
        assert value0 == 1, f"Expected 1 for depth=0, got {value0}"

        # Test with depth=1 (one level of recursion)
        logger.info("[TEST] Testing depth=1")
        start_time = time.time()
        result1 = service.compute_recursive(1)
        value1 = result1.get()
        logger.info(f"[TEST] depth=1 result={value1} ({time.time() - start_time:.2f}s)")
        assert value1 == 2, f"Expected 2 for depth=1, got {value1}"

        # Test with depth=2 (two levels of recursion)
        logger.info("[TEST] Testing depth=2")
        start_time = time.time()
        result2 = service.compute_recursive(2)
        value2 = result2.get()
        logger.info(f"[TEST] depth=2 result={value2} ({time.time() - start_time:.2f}s)")
        assert value2 == 4, f"Expected 4 for depth=2, got {value2}"


# =============================================================================
# Flmrun Application Tests (from test_flmrun.py)
# =============================================================================


FLMRUN_E2E_APP = "flmrun-e2e"


@pytest.fixture
def setup_flmrun_with_e2e():
    """
    Fixture to register a flmrun application with e2e modules available.

    This registers a custom flmrun application with PYTHONPATH set to include
    the e2e package, making e2e modules available to the runner.
    """
    import os

    if not os.path.exists("/opt/e2e"):
        pytest.skip("Requires /opt/e2e directory (Docker E2E environment only)")

    flmrun = flamepy.get_application("flmrun")

    flamepy.register_application(
        FLMRUN_E2E_APP,
        flamepy.ApplicationAttributes(
            working_directory="/opt/e2e",
            command=flmrun.command,
            arguments=flmrun.arguments,
            environments={"PYTHONPATH": "/opt/e2e/src"},
            installer="python",
            description="Flmrun with e2e modules available",
        ),
    )

    yield

    flamepy.unregister_application(FLMRUN_E2E_APP)


@pytest.mark.skipif(not os.path.exists("/opt/e2e"), reason="Requires Docker E2E environment")
class TestFlmrunApplication:
    """Tests for flmrun application functionality."""

    def test_flmrun_application_registered(self, setup_flmrun_with_e2e):
        """Test that flmrun is registered as a default application."""
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert FLMRUN_E2E_APP in app_names, f"{FLMRUN_E2E_APP} not found in applications: {app_names}"

        flmrun = flamepy.get_application(FLMRUN_E2E_APP)
        assert flmrun.name == FLMRUN_E2E_APP
        assert flmrun.state == flamepy.ApplicationState.ENABLED
        assert flmrun.command.endswith("/bin/uv")
        assert flmrun.arguments[:4] == [
            "run",
            "--python",
            "python${FLAME_PYTHON_VERSION}",
            "python",
        ]

    def test_flmrun_sum_function(self, setup_flmrun_with_e2e):
        """Test Case 1: Run a simple sum function remotely."""
        from e2e.helpers import serialize_runner_context, serialize_runner_request

        ctx = runner.RunnerContext(execution_object=sum_func)
        common_data_bytes = serialize_runner_context(ctx, FLMRUN_E2E_APP)
        ssn = flamepy.create_session(FLMRUN_E2E_APP, common_data_bytes)

        try:
            req = runner.RunnerRequest(method=None, args=(1, 2))
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)

            result_ref = flamepy.core.ObjectRef.decode(result_bytes)
            result = flamepy.core.get_object(result_ref)

            assert result == 3, f"Expected 3, got {result}"
        finally:
            ssn.close()

    def test_flmrun_class_method(self, setup_flmrun_with_e2e):
        """Test Case 2: Run methods on a class instance."""
        from e2e.helpers import serialize_runner_context, serialize_runner_request

        calc = Calculator()

        ctx = runner.RunnerContext(execution_object=calc)
        common_data_bytes = serialize_runner_context(ctx, FLMRUN_E2E_APP)
        ssn = flamepy.create_session(FLMRUN_E2E_APP, common_data_bytes)

        try:
            req = runner.RunnerRequest(method="add", args=(5, 3))
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 8, f"Expected 8, got {result}"

            req = runner.RunnerRequest(method="multiply", args=(4, 7))
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 28, f"Expected 28, got {result}"

            req = runner.RunnerRequest(method="subtract", args=(10, 3))
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 7, f"Expected 7, got {result}"
        finally:
            ssn.close()

    def test_flmrun_kwargs(self, setup_flmrun_with_e2e):
        """Test Case 3: Run a function with keyword arguments."""
        from e2e.helpers import serialize_runner_context, serialize_runner_request

        ctx = runner.RunnerContext(execution_object=greet_func)
        common_data_bytes = serialize_runner_context(ctx, FLMRUN_E2E_APP)
        ssn = flamepy.create_session(FLMRUN_E2E_APP, common_data_bytes)

        try:
            req = runner.RunnerRequest(method=None, kwargs={"name": "World", "greeting": "Hi"})
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == "Hi, World!", f"Expected 'Hi, World!', got {result}"

            req = runner.RunnerRequest(method=None, kwargs={"name": "Python"})
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == "Hello, Python!", f"Expected 'Hello, Python!', got {result}"
        finally:
            ssn.close()

    def test_flmrun_stateful_class(self, setup_flmrun_with_e2e):
        """Test Case 6: Run a stateful class with instance variables."""
        from e2e.helpers import serialize_runner_context, serialize_runner_request

        counter = Counter()

        ctx = runner.RunnerContext(execution_object=counter)
        common_data_bytes = serialize_runner_context(ctx, FLMRUN_E2E_APP)
        ssn = flamepy.create_session(FLMRUN_E2E_APP, common_data_bytes)

        try:
            req = runner.RunnerRequest(method="increment")
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 1, f"Expected 1, got {result}"

            req = runner.RunnerRequest(method="increment")
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 2, f"Expected 2, got {result}"

            req = runner.RunnerRequest(method="add", args=(5,))
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 7, f"Expected 7, got {result}"

            req = runner.RunnerRequest(method="get_count")
            req_bytes = serialize_runner_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 7, f"Expected 7, got {result}"
        finally:
            ssn.close()


# =============================================================================
# get_data Helper Tests (from test_get_data.py)
# =============================================================================


class TestGetData:
    """Tests for the `get_data` helper function in flamepy.runner."""

    def test_get_data_task_input_positional_args(self, check_package_config, check_flmrun_app):
        """TC-GD-001: Test get_data retrieves task input with positional arguments."""
        from flamepy.core import get_session
        from flamepy.runner import get_data

        with runner.Runner("test-get-data-input-pos") as rr:
            sum_service = rr.service(sum_func)

            result = sum_service(5, 3)
            value = result.get()
            assert value == 8, f"Expected 8, got {value}"

            session = get_session(sum_service._session.id)
            tasks = list(session.list_tasks())
            assert len(tasks) >= 1, "Expected at least one task"

            task = tasks[0]
            assert task.input is not None, "Task input should not be None"

            input_data = get_data(task.input)

            assert input_data["type"] == "input", f"Expected type 'input', got {input_data['type']}"
            assert input_data["method"] is None, f"Expected method None for function, got {input_data['method']}"
            assert input_data["args"] == (5, 3), f"Expected args (5, 3), got {input_data['args']}"

    def test_get_data_task_output(self, check_package_config, check_flmrun_app):
        """TC-GD-002: Test get_data retrieves task output correctly."""
        from flamepy.core import get_session
        from flamepy.runner import get_data

        from e2e.helpers import multiply_func

        with runner.Runner("test-get-data-output") as rr:
            multiply_service = rr.service(multiply_func)

            result = multiply_service(4, 7)
            value = result.get()
            assert value == 28, f"Expected 28, got {value}"

            session = get_session(multiply_service._session.id)
            tasks = list(session.list_tasks())
            assert len(tasks) >= 1, "Expected at least one task"

            task = tasks[0]
            assert task.output is not None, "Task output should not be None"

            output_data = get_data(task.output)

            assert output_data["type"] == "output", f"Expected type 'output', got {output_data['type']}"
            assert output_data["result"] == 28, f"Expected result 28, got {output_data['result']}"

    def test_get_data_invalid_data_format(self, check_package_config, check_flmrun_app):
        """TC-GD-007: Test get_data handles invalid data format gracefully."""
        from flamepy.runner import ErrorType, RunnerError, get_data

        invalid_data = b"this is not valid objectref data"

        with pytest.raises(RunnerError) as exc_info:
            get_data(invalid_data)

        assert exc_info.value.error_type == ErrorType.DECODE_ERROR
        assert "decode" in str(exc_info.value).lower() or "failed" in str(exc_info.value).lower()

    def test_get_data_empty_bytes(self, check_package_config, check_flmrun_app):
        """TC-GD-008: Test get_data handles empty bytes gracefully."""
        from flamepy.runner import ErrorType, RunnerError, get_data

        empty_data = b""

        with pytest.raises(RunnerError) as exc_info:
            get_data(empty_data)

        assert exc_info.value.error_type == ErrorType.DECODE_ERROR

    def test_get_data_class_method_input(self, check_package_config, check_flmrun_app):
        """TC-GD-006: Test get_data retrieves class method invocation input."""
        from flamepy.core import get_session
        from flamepy.runner import get_data

        with runner.Runner("test-get-data-method") as rr:
            calc_service = rr.service(Calculator())

            result = calc_service.add(15, 25)
            value = result.get()
            assert value == 40, f"Expected 40, got {value}"

            session = get_session(calc_service._session.id)
            tasks = list(session.list_tasks())
            assert len(tasks) >= 1, "Expected at least one task"

            task = tasks[0]
            assert task.input is not None, "Task input should not be None"

            input_data = get_data(task.input)

            assert input_data["type"] == "input"
            assert input_data["method"] == "add", f"Expected method 'add', got {input_data['method']}"
            assert input_data["args"] == (15, 25), f"Expected args (15, 25), got {input_data['args']}"


# =============================================================================
# Distributed Running Functions Tests (from test_drf.py)
# =============================================================================


class TestParallelExecution:
    """Tests for parallel task execution patterns."""

    def test_parallel_tasks_basic(self, check_package_config, check_flmrun_app):
        """Test basic parallel task execution with multiple tasks submitted at once."""
        with runner.Runner("test-drf-parallel-basic") as rr:
            sum_service = rr.service(sum_func)

            results = [
                sum_service(1, 1),
                sum_service(2, 2),
                sum_service(3, 3),
                sum_service(4, 4),
                sum_service(5, 5),
            ]

            values = rr.get(results)
            assert values == [2, 4, 6, 8, 10], f"Expected [2, 4, 6, 8, 10], got {values}"

    def test_parallel_tasks_high_concurrency(self, check_package_config, check_flmrun_app):
        """Test high concurrency with many parallel tasks."""
        with runner.Runner("test-drf-parallel-high") as rr:
            sum_service = rr.service(sum_func)

            num_tasks = 50
            results = [sum_service(i, i) for i in range(num_tasks)]
            values = rr.get(results)

            expected = [i * 2 for i in range(num_tasks)]
            assert values == expected, "High concurrency test failed"

    def test_parallel_tasks_different_services(self, check_package_config, check_flmrun_app):
        """Test parallel execution across different services."""
        with runner.Runner("test-drf-parallel-multi-svc") as rr:
            sum_service = rr.service(sum_func)
            calc_service = rr.service(Calculator())

            results = [
                sum_service(10, 5),
                calc_service.multiply(3, 4),
                sum_service(20, 10),
                calc_service.subtract(15, 5),
            ]

            values = rr.get(results)
            assert values == [15, 12, 30, 10], f"Expected [15, 12, 30, 10], got {values}"

    def test_parallel_select_iterator(self, check_package_config, check_flmrun_app):
        """Test using select() iterator for parallel task results."""
        with runner.Runner("test-drf-parallel-select") as rr:
            sum_service = rr.service(sum_func)

            results = [
                sum_service(1, 2),
                sum_service(3, 4),
                sum_service(5, 6),
            ]

            completed_values = []
            for result in rr.select(results):
                completed_values.append(result.get())

            assert sorted(completed_values) == [3, 7, 11]


class TestTaskChaining:
    """Tests for task chaining and dependency patterns."""

    def test_task_chaining_sequential(self, check_package_config, check_flmrun_app):
        """Test sequential task chaining where output of one task feeds into next."""
        with runner.Runner("test-drf-chain-seq") as rr:
            counter = Counter()
            cnt_service = rr.service(counter)

            cnt_service.add(10).wait()
            cnt_service.add(5).wait()
            cnt_service.increment().wait()
            result = cnt_service.get_count()

            value = result.get()
            assert value == 16, f"Expected 16, got {value}"

    def test_task_chaining_with_objectfuture(self, check_package_config, check_flmrun_app):
        """Test chaining using ObjectFuture as argument to next task."""
        with runner.Runner("test-drf-chain-objfuture") as rr:
            counter = Counter()
            cnt_service = rr.service(counter)

            cnt_service.add(10).wait()
            intermediate = cnt_service.get_count()

            cnt_service.add(intermediate).wait()
            final = cnt_service.get_count()

            value = final.get()
            assert value == 20, f"Expected 20, got {value}"

    def test_task_dependency_graph(self, check_package_config, check_flmrun_app):
        """Test dependency graph: a(1,2)=3, b(3,4)=7, c(a,b)=10."""
        with runner.Runner("test-drf-chain-graph") as rr:
            sum_service = rr.service(sum_func)

            a = sum_service(1, 2)
            b = sum_service(3, 4)

            val_a, val_b = rr.get([a, b])
            assert val_a == 3
            assert val_b == 7

            c = sum_service(val_a, val_b)
            val_c = c.get()
            assert val_c == 10, f"Expected 10, got {val_c}"


class TestMapReducePattern:
    """Tests for map-reduce distributed computing patterns."""

    def test_map_phase(self, check_package_config, check_flmrun_app):
        """Test map phase - apply same operation to multiple inputs."""
        with runner.Runner("test-drf-map") as rr:
            calc_service = rr.service(Calculator())

            inputs = [2, 3, 4, 5, 6]
            mapped_results = [calc_service.multiply(x, x) for x in inputs]

            values = rr.get(mapped_results)
            assert values == [4, 9, 16, 25, 36], f"Map phase failed: {values}"

    def test_reduce_phase(self, check_package_config, check_flmrun_app):
        """Test reduce phase - aggregate multiple values pairwise."""
        with runner.Runner("test-drf-reduce") as rr:
            sum_service = rr.service(sum_func)

            values = [10, 20, 30, 40]

            level1 = [
                sum_service(values[0], values[1]),
                sum_service(values[2], values[3]),
            ]
            level1_values = rr.get(level1)
            assert level1_values == [30, 70]

            result = sum_service(level1_values[0], level1_values[1])
            final = result.get()
            assert final == 100, f"Reduce phase failed: {final}"

    def test_full_map_reduce(self, check_package_config, check_flmrun_app):
        """Test map-reduce: square numbers [1,2,3,4] then sum = 1+4+9+16 = 30."""
        with runner.Runner("test-drf-mapreduce") as rr:
            calc_service = rr.service(Calculator())
            sum_service = rr.service(sum_func)

            inputs = [1, 2, 3, 4]

            mapped = [calc_service.multiply(x, x) for x in inputs]
            squared = rr.get(mapped)
            assert squared == [1, 4, 9, 16], f"Map failed: {squared}"

            level1 = [
                sum_service(squared[0], squared[1]),
                sum_service(squared[2], squared[3]),
            ]
            level1_values = rr.get(level1)

            final = sum_service(level1_values[0], level1_values[1])
            result = final.get()
            assert result == 30, f"MapReduce result should be 30, got {result}"


class TestDRFErrorHandling:
    """Tests for error handling in distributed execution."""

    def test_error_in_single_task(self, check_package_config, check_flmrun_app):
        """Test that errors in a single task are properly propagated."""

        def failing_func(x: int) -> int:
            if x < 0:
                raise ValueError(f"Negative value not allowed: {x}")
            return x * 2

        with runner.Runner("test-drf-error-single") as rr:
            service = rr.service(failing_func)

            result = service(5)
            assert result.get() == 10

            error_result = service(-1)
            with pytest.raises(Exception):
                error_result.get()

    def test_partial_failure_in_parallel(self, check_package_config, check_flmrun_app):
        """Test handling when some tasks fail in parallel execution."""

        def conditional_fail(x: int) -> int:
            if x == 3:
                raise ValueError("Task 3 always fails")
            return x * 10

        with runner.Runner("test-drf-error-partial") as rr:
            service = rr.service(conditional_fail)

            results = [
                service(1),
                service(2),
                service(3),
                service(4),
            ]

            successful_values = []
            failed_count = 0
            for result in results:
                try:
                    successful_values.append(result.get())
                except Exception:
                    failed_count += 1

            assert len(successful_values) == 3
            assert sorted(successful_values) == [10, 20, 40]
            assert failed_count == 1


class TestDRFStatefulServices:
    """Tests for stateful service behavior in DRF."""

    def test_stateful_counter_operations(self, check_package_config, check_flmrun_app):
        """Test stateful counter with multiple operations."""
        with runner.Runner("test-drf-stateful-counter") as rr:
            counter = Counter()
            cnt_service = rr.service(counter)

            cnt_service.add(100).wait()
            cnt_service.increment().wait()
            cnt_service.increment().wait()
            cnt_service.add(50).wait()

            result = cnt_service.get_count()
            value = result.get()
            assert value == 152, f"Expected 152, got {value}"

    def test_stateful_isolation_between_services(self, check_package_config, check_flmrun_app):
        """Test that different stateful services maintain separate state."""
        with runner.Runner("test-drf-stateful-isolation") as rr:
            counter1 = Counter()
            counter2 = Counter()

            svc1 = rr.service(counter1)
            svc2 = rr.service(counter2)

            svc1.add(10).wait()
            svc1.increment().wait()

            svc2.add(100).wait()

            val1 = svc1.get_count().get()
            val2 = svc2.get_count().get()

            assert val1 == 11, f"Counter1 expected 11, got {val1}"
            assert val2 == 100, f"Counter2 expected 100, got {val2}"


class TestDRFSessionManagement:
    """Tests for session lifecycle and management in DRF."""

    def test_session_cleanup_on_exit(self, check_package_config, check_flmrun_app):
        """Test that session is properly cleaned up when Runner exits."""
        app_name = "test-drf-session-cleanup"

        with runner.Runner(app_name) as rr:
            sum_service = rr.service(sum_func)
            result = sum_service(1, 2)
            assert result.get() == 3

            session_id = sum_service._session.id

        sessions = flamepy.list_sessions()
        session = next((s for s in sessions if s.id == session_id), None)
        if session:
            assert session.state == flamepy.SessionState.CLOSED

    def test_multiple_runners_same_app(self, check_package_config, check_flmrun_app):
        """Test running multiple Runners with the same application name."""
        app_name = "test-drf-multi-runner"

        with runner.Runner(app_name) as rr1:
            svc1 = rr1.service(sum_func)
            r1 = svc1(10, 20)
            val1 = r1.get()
            assert val1 == 30

            with runner.Runner(app_name) as rr2:
                svc2 = rr2.service(sum_func)
                r2 = svc2(100, 200)
                val2 = r2.get()
                assert val2 == 300


class TestDRFPerformance:
    """Tests for performance characteristics in DRF."""

    def test_throughput_many_small_tasks(self, check_package_config, check_flmrun_app):
        """Test throughput with many small tasks."""
        import time as time_module

        with runner.Runner("test-drf-throughput") as rr:
            sum_service = rr.service(sum_func)

            start_time = time_module.time()

            num_tasks = 100
            results = [sum_service(i, 1) for i in range(num_tasks)]
            values = rr.get(results)

            elapsed = time_module.time() - start_time

            expected = [i + 1 for i in range(num_tasks)]
            assert values == expected

            throughput = num_tasks / elapsed if elapsed > 0 else 0
            print(f"Throughput: {throughput:.2f} tasks/sec for {num_tasks} tasks in {elapsed:.2f}s")


class TestDRFEdgeCases:
    """Tests for edge cases and boundary conditions in DRF."""

    def test_empty_arguments(self, check_package_config, check_flmrun_app):
        """Test calling function with no arguments."""

        def get_constant() -> int:
            return 42

        with runner.Runner("test-drf-empty-args") as rr:
            service = rr.service(get_constant)
            result = service()
            value = result.get()
            assert value == 42

    def test_none_arguments(self, check_package_config, check_flmrun_app):
        """Test handling None as argument."""

        def handle_none(x) -> str:
            return "none" if x is None else "not-none"

        with runner.Runner("test-drf-none-args") as rr:
            service = rr.service(handle_none)
            result = service(None)
            value = result.get()
            assert value == "none"

    def test_large_return_value(self, check_package_config, check_flmrun_app):
        """Test handling large return values."""

        def create_large_list(n: int) -> list:
            return list(range(n))

        with runner.Runner("test-drf-large-return") as rr:
            service = rr.service(create_large_list)
            result = service(10000)
            value = result.get()
            assert len(value) == 10000
            assert value[0] == 0
            assert value[9999] == 9999

    def test_nested_data_structures(self, check_package_config, check_flmrun_app):
        """Test handling nested data structures."""

        def process_nested(data: dict) -> dict:
            return {
                "input": data,
                "processed": True,
                "nested": {"level": 2, "data": [1, 2, 3]},
            }

        with runner.Runner("test-drf-nested-data") as rr:
            service = rr.service(process_nested)
            input_data = {"key": "value", "list": [1, 2, 3]}
            result = service(input_data)
            value = result.get()

            assert value["processed"] is True
            assert value["input"] == input_data
            assert value["nested"]["level"] == 2


class TestDRFConcurrentAccess:
    """Tests for concurrent access patterns in DRF."""

    def test_concurrent_service_calls(self, check_package_config, check_flmrun_app):
        """Test concurrent calls to the same service."""
        with runner.Runner("test-drf-concurrent-calls") as rr:
            sum_service = rr.service(sum_func)

            num_calls = 30
            results = [sum_service(i, i + 1) for i in range(num_calls)]

            values = rr.get(results)
            expected = [i + (i + 1) for i in range(num_calls)]
            assert values == expected

    def test_interleaved_operations(self, check_package_config, check_flmrun_app):
        """Test interleaved operations on multiple services."""
        with runner.Runner("test-drf-interleaved") as rr:
            sum_service = rr.service(sum_func)
            calc_service = rr.service(Calculator())

            results = []
            for i in range(10):
                results.append(sum_service(i, 1))
                results.append(calc_service.multiply(i, 2))

            values = rr.get(results)

            for i in range(10):
                sum_idx = i * 2
                mult_idx = i * 2 + 1
                assert values[sum_idx] == i + 1, f"Sum at {sum_idx} wrong"
                assert values[mult_idx] == i * 2, f"Multiply at {mult_idx} wrong"
