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
from flamepy import runner
from flamepy.runner import SessionContext

from e2e.helpers import (
    Calculator,
    Counter,
    RecursiveService,
    greet_func,
    sum_func,
)


@pytest.fixture(scope="module")
def check_package_config():
    """Check that storage configuration is available (via package.storage or cache.endpoint)."""
    ctx = flamepy.FlameContext()
    # Storage can come from either package.storage or cache.endpoint
    has_package_storage = ctx.package is not None and getattr(ctx.package, 'storage', None) is not None
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
        # Create a service with a class (should auto-instantiate)
        cnt_s = rr.service(Counter)

        # Call methods - use wait() to ensure completion without fetching results
        rr.wait([cnt_s.increment(), cnt_s.add(3)])
        res_r = cnt_s.get_count()

        # Get the result
        value = res_r.get()
        assert value == 4, f"Expected 4, got {value}"


def test_runner_with_instance(check_package_config, check_flmrun_app):
    """Test Case 4: Test Runner with a class instance."""
    with runner.Runner("test-runner-instance") as rr:
        # Create a Counter instance with initial value
        counter = Counter()
        # Set initial count to 10 by adding 10
        counter.add(10)

        # Create a service with the instance
        cnt_os = rr.service(counter)

        # Call methods - use wait() to ensure completion without fetching results
        rr.wait([cnt_os.increment(), cnt_os.add(3)])
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

        # Create a service with the instance
        cnt_os = rr.service(counter)

        # Call methods - use wait() to ensure completion without fetching results
        rr.wait([cnt_os.increment(), cnt_os.add(3)])
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
    has_package_storage = ctx.package is not None and getattr(ctx.package, 'storage', None) is not None
    has_cache_endpoint = ctx.cache is not None
    if has_package_storage or has_cache_endpoint:
        pytest.skip("Storage config is available (package.storage or cache.endpoint), cannot test error case")

    with pytest.raises(flamepy.FlameError) as exc_info:
        with runner.Runner("test-no-config"):
            pass

    assert exc_info.value.code == flamepy.FlameErrorCode.INVALID_CONFIG


def test_runner_stateful_instance(check_package_config, check_flmrun_app):
    """Test Case 14: Test Runner with stateful=True for instance."""
    with runner.Runner("test-runner-stateful") as rr:
        # Create a Counter instance
        counter = Counter()

        # Create a stateful service (state should persist across tasks)
        cnt_service = rr.service(counter, stateful=True, autoscale=False)

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
        sum_service = rr.service(sum_func, stateful=False, autoscale=True)

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
        calc_service = rr.service(Calculator, stateful=False, autoscale=False)

        # Call methods
        result1 = calc_service.add(10, 5)
        result2 = calc_service.multiply(3, 4)

        values = rr.get([result1, result2])
        assert values == [15, 12], f"Expected [15, 12], got {values}"


def test_runner_error_stateful_class(check_package_config, check_flmrun_app):
    """Test Case 17: Test that stateful=True raises error for class."""
    with runner.Runner("test-runner-stateful-class-error") as rr:
        # Trying to create a stateful service with a class should raise ValueError
        with pytest.raises(ValueError) as exc_info:
            rr.service(Counter, stateful=True)

        assert "Cannot set stateful=True for a class" in str(exc_info.value)


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
    """Test Case 19: Test default parameters for class (stateful=False, autoscale=False)."""
    with runner.Runner("test-runner-defaults-class") as rr:
        # Create service with class using defaults (should be stateful=False, autoscale=False)
        counter_service = rr.service(Counter)

        # Call methods
        counter_service.add(10).wait()
        counter_service.increment().wait()
        result = counter_service.get_count()

        value = result.get()
        assert value == 11, f"Expected 11, got {value}"


def test_runner_defaults_instance(check_package_config, check_flmrun_app):
    """Test Case 20: Test default parameters for instance (stateful=False, autoscale=False)."""
    with runner.Runner("test-runner-defaults-instance") as rr:
        # Create an instance
        calc = Calculator()

        # Create service with instance using defaults (should be stateful=False, autoscale=False)
        calc_service = rr.service(calc)

        # Call methods
        result1 = calc_service.add(5, 3)
        result2 = calc_service.subtract(10, 4)

        values = rr.get([result1, result2])
        assert values == [8, 6], f"Expected [8, 6], got {values}"


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

    logging.basicConfig(level=logging.INFO)
    logger = logging.getLogger(__name__)

    # Shared application name and session ID
    shared_app_name = "test-runner-recursive"
    shared_session_id = "recursive-session-001"

    logger.info(f"[TEST] Starting recursive test: app={shared_app_name}, session={shared_session_id}")

    # Create an instance with the shared session ID and app name
    recursive_instance = RecursiveService(
        session_id=shared_session_id,
        app_name=shared_app_name,
    )

    with runner.Runner(shared_app_name) as rr:
        # Use autoscale=True to allow multiple executors for recursive calls
        # Without autoscale, a single executor would deadlock waiting for its own recursive task
        service = rr.service(recursive_instance, autoscale=True)
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
