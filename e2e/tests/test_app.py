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
import socket
import tempfile
import time
import uuid
from contextlib import contextmanager
from pathlib import Path

import flamepy
import flamepy.app as app
import pytest
from flamepy import TaskOptions
from flamepy.app.helper import Error as DataError
from flamepy.app.helper import ErrorType as DataErrorType
from flamepy.app.helper import get_data
from flamepy.proto.types_pb2 import ExecutorBound, ExecutorIdle

from e2e.helpers import (
    Calculator,
    Counter,
    RecursiveService,
    greet_func,
    sum_func,
)
from tests.utils import wait_for_application_deleted


@contextmanager
def initialized_app(name, **kwargs):
    """Initialize one process-wide app for an E2E workflow."""
    app.init(name, **kwargs)
    try:
        yield app
    finally:
        app.destroy()


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


def test_app_lifecycle_fixture(check_package_config, check_flmrun_app):
    """Test Case 1: Test the E2E application lifecycle fixture."""
    with initialized_app("test-app-cm"):
        # Verify that the application is registered
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-app-cm" in app_names, f"test-app-cm not found in applications: {app_names}"

    wait_for_application_deleted("test-app-cm")

    # After cleanup reconciliation, the application is physically removed.
    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-app-cm" not in app_names, f"test-app-cm should be unregistered but found in: {app_names}"


def test_app_with_function(check_package_config, check_flmrun_app):
    """Test Case 2: Test App with a simple function."""
    with initialized_app("test-app-func"):

        @app.service(resreq="cpu=1")
        def sum_service(left, right):
            return left + right

        # Call the function remotely
        result = sum_service(1, 3)

        # Verify result is an ObjectFuture
        assert isinstance(result, app.ObjectFuture), f"Expected ObjectFuture, got {type(result)}"

        # Get the actual result
        value = result.get()
        assert value == 4, f"Expected 4, got {value}"


def test_app_with_class(check_package_config, check_flmrun_app):
    """Test Case 3: Test App with a class (auto-instantiation)."""
    with initialized_app("test-app-class"):

        @app.service()
        class CalculatorService:
            def add(self, a, b):
                return a + b

            def multiply(self, a, b):
                return a * b

            def subtract(self, a, b):
                return a - b

        calculator = CalculatorService()
        res_r = calculator.multiply(2, 3)

        # Get the result
        value = res_r.get()
        assert value == 6, f"Expected 6, got {value}"


def test_decorated_class_creates_service_instance(
    check_package_config,
    check_flmrun_app,
):
    """Constructing a decorated class creates an executor-backed instance."""
    with initialized_app("test-app-class-instance"):

        @app.service(autoscale=False, warmup=1)
        class CounterService:
            def __init__(self, count=0):
                self.count = count

            def increment(self):
                self.count += 1
                return self.count

            def get_count(self):
                return self.count

        counter = CounterService(10)
        assert counter.increment().get() == 11
        assert counter.get_count().get() == 11


def test_app_data_aware_scheduling(check_package_config, check_flmrun_app):
    """DAS rebinds the matching one of two retained App processes."""

    def wait_for_executors(predicate, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            matched = [executor for executor in flamepy.list_executors() if predicate(executor)]
            if len(matched) == 2:
                return matched
            time.sleep(0.1)
        pytest.fail("timed out waiting for two DAS test executors")

    with initialized_app("test-app-das"):

        @app.service(warmup=2)
        class WarmDataService:
            def __init__(self):
                self.instance_key = f"e2e:data-aware:{socket.gethostname()}:{os.getpid()}".encode()
                endpoint = Path(os.environ["FLAME_INSTANCE_ENDPOINT"])
                self.executor_id = endpoint.parent.name if endpoint.name == "instance.sock" else endpoint.stem

            def run(self, value, delay=0):
                app.publish_attributes({self.instance_key})
                if delay:
                    time.sleep(delay)
                return value, self.instance_key, self.executor_id

        warm_service = WarmDataService()
        warm_session_id = warm_service._session.id
        bound = wait_for_executors(lambda executor: executor.status.state == ExecutorBound and executor.status.session_id == warm_session_id)

        executor_ids = {executor.metadata.id for executor in bound}
        key_by_executor = {}
        probe_deadline = time.monotonic() + 60
        attempt = 0
        while key_by_executor.keys() != executor_ids and time.monotonic() < probe_deadline:
            probes = [warm_service.run(f"warmup-{attempt}-{index}", delay=1) for index in range(2)]
            for probe in probes:
                _, key, executor_id = probe.get()
                key_by_executor[executor_id] = key
            attempt += 1
        assert key_by_executor.keys() == executor_ids, "not all bound DAS executors accepted a warmup probe"

        # Create the target sessions before releasing the warm session so the
        # affinity tasks can be submitted within the bounded Idle reuse window.
        target_executor_ids = sorted(executor_ids, reverse=True)
        targets = [key_by_executor[executor_id] for executor_id in target_executor_ids]

        def declare_data_service():
            @app.service(warmup=0)
            class TargetDataService:
                def __init__(self):
                    self.instance_key = f"e2e:data-aware:{socket.gethostname()}:{os.getpid()}".encode()
                    endpoint = Path(os.environ["FLAME_INSTANCE_ENDPOINT"])
                    self.executor_id = endpoint.parent.name if endpoint.name == "instance.sock" else endpoint.stem

                def run(self, value, delay=0):
                    app.publish_attributes({self.instance_key})
                    if delay:
                        time.sleep(delay)
                    return value, self.instance_key, self.executor_id

            return TargetDataService()

        services = [declare_data_service() for _ in targets]
        warm_service.close()
        wait_for_executors(lambda executor: executor.metadata.id in executor_ids and executor.status.state == ExecutorIdle)

        # Let several scheduler cycles pass. Without the Idle grace, Shuffle
        # would release these retained App processes before DAS can reuse them.
        time.sleep(2)
        retained = [executor for executor in flamepy.list_executors() if executor.metadata.id in executor_ids]
        assert {executor.metadata.id for executor in retained} == executor_ids
        assert all(executor.status.state == ExecutorIdle for executor in retained)

        futures = [
            service.run(
                f"affinity-match-{index}",
                option=TaskOptions(affinity={target}),
            )
            for index, (service, target) in enumerate(zip(services, targets))
        ]

        for index, (future, target, executor_id) in enumerate(zip(futures, targets, target_executor_ids)):
            value, selected_key, selected_executor_id = future.get()
            assert value == f"affinity-match-{index}"
            assert selected_key == target
            assert selected_executor_id == executor_id


def test_app_with_instance(check_package_config, check_flmrun_app):
    """Test Case 4: Test App with a class instance."""
    with initialized_app("test-app-instance"):

        @app.service(autoscale=False, warmup=1)
        class CounterService:
            def __init__(self):
                self.count = 10

            def increment(self):
                self.count += 1
                return self.count

            def get_count(self):
                return self.count

            def add(self, value):
                self.count += value
                return self.count

        cnt_os = CounterService()

        # Apply state changes sequentially so the expected total is deterministic.
        cnt_os.increment().wait()
        cnt_os.add(3).wait()
        res_r = cnt_os.get_count()

        # Get the result
        value = res_r.get()
        assert value == 14, f"Expected 14, got {value}"


def test_app_with_objectfuture_args(check_package_config, check_flmrun_app):
    """Test Case 5: Test App with ObjectFuture as arguments."""
    with initialized_app("test-app-objfuture"):

        @app.service(autoscale=False, warmup=1)
        class CounterService:
            def __init__(self):
                self.count = 10

            def increment(self):
                self.count += 1
                return self.count

            def get_count(self):
                return self.count

            def add(self, value):
                self.count += value
                return self.count

        cnt_os = CounterService()

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


def test_app_multiple_services(check_package_config, check_flmrun_app):
    """Test Case 6: Test App with multiple services."""
    with initialized_app("test-app-multi"):

        @app.service()
        def sum_service(left, right):
            return left + right

        @app.service()
        class CalculatorService:
            def add(self, a, b):
                return a + b

            def multiply(self, a, b):
                return a * b

            def subtract(self, a, b):
                return a - b

        # Call methods on different services
        result1 = sum_service(5, 3)
        result2 = CalculatorService().multiply(4, 7)

        # Get results
        value1, value2 = app.get([result1, result2])

        assert value1 == 8, f"Expected 8, got {value1}"
        assert value2 == 28, f"Expected 28, got {value2}"


def test_app_with_kwargs(check_package_config, check_flmrun_app):
    """Test Case 7: Test App with keyword arguments."""
    with initialized_app("test-app-kwargs"):

        @app.service()
        def greet_service(name, greeting="Hello"):
            return f"{greeting}, {name}!"

        # Call with keyword arguments
        result = greet_service(name="World", greeting="Hi")

        # Call with partial kwargs (uses default)
        result2 = greet_service(name="Python")

        value1, value2 = app.get([result, result2])
        assert value1 == "Hi, World!", f"Expected 'Hi, World!', got {value1}"
        assert value2 == "Hello, Python!", f"Expected 'Hello, Python!', got {value2}"


def test_app_package_excludes(check_package_config, check_flmrun_app):
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

            # Use App (should exclude .log, .pkl, __pycache__)
            with initialized_app("test-app-excludes"):
                # Just verify it works - the exclusion is tested by successful packaging
                pass

        finally:
            # Restore original directory
            os.chdir(original_dir)


def test_objectfuture_ref_method(check_package_config, check_flmrun_app):
    """Test Case 9: Test ObjectFuture.ref() method."""
    with initialized_app("test-objectfuture-ref"):

        @app.service()
        def sum_service(left, right):
            return left + right

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
    with initialized_app("test-objectfuture-iterator"):

        @app.service()
        def sum_service(left, right):
            return left + right

        results = [
            sum_service(1, 2),
            sum_service(5, 7),
            sum_service(3, 4),
        ]

        values = []
        for result in app.select(results):
            values.append(result.get())

        assert sorted(values) == [3, 7, 12]


def test_app_service_close(check_package_config, check_flmrun_app):
    """Test Case 11: Test that ServiceInstance.close() works."""
    with initialized_app("test-service-close"):

        @app.service()
        def sum_service(left, right):
            return left + right

        # Use the service
        result = sum_service(1, 2)
        assert result.get() == 3

        sum_service.close()
        with pytest.raises(flamepy.FlameError, match="closed"):
            sum_service(2, 3)


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


def test_app_error_no_storage_config():
    """Test App fails gracefully without storage config (no package.storage and no cache.endpoint)."""
    ctx = flamepy.FlameContext()
    has_package_storage = ctx.package is not None and getattr(ctx.package, "storage", None) is not None
    has_cache_endpoint = ctx.cache is not None
    if has_package_storage or has_cache_endpoint:
        pytest.skip("Storage config is available (package.storage or cache.endpoint), cannot test error case")

    with pytest.raises(flamepy.FlameError) as exc_info:
        with initialized_app("test-no-config"):
            pass

    assert exc_info.value.code == flamepy.FlameErrorCode.INVALID_CONFIG


def test_app_retained_class_handle(check_package_config, check_flmrun_app):
    """Test Case 14: Test state retained by a constructed class handle."""
    with initialized_app("test-app-retained"):

        @app.service(autoscale=False, warmup=1)
        class CounterService:
            def __init__(self):
                self.count = 0

            def increment(self):
                self.count += 1
                return self.count

            def get_count(self):
                return self.count

            def add(self, value):
                self.count += value
                return self.count

        cnt_service = CounterService()

        # Call methods
        cnt_service.add(5).wait()
        cnt_service.increment().wait()
        result = cnt_service.get_count()

        # Get the result
        value = result.get()
        assert value == 6, f"Expected 6, got {value}"


def test_app_stateless_function(check_package_config, check_flmrun_app):
    """Test Case 15: Test App with stateless function (default behavior)."""
    with initialized_app("test-app-stateless-func"):

        @app.service()
        def sum_service(left, right):
            return left + right

        # Call the function multiple times
        results = [sum_service(i, i + 1) for i in range(5)]
        values = app.get(results)

        # Verify results
        expected = [1, 3, 5, 7, 9]
        assert values == expected, f"Expected {expected}, got {values}"


def test_app_class_single_instance(check_package_config, check_flmrun_app):
    """Test Case 16: Test App with class and autoscale=False (single instance)."""
    with initialized_app("test-app-class-single"):

        @app.service(autoscale=False)
        class CalculatorService:
            def add(self, a, b):
                return a + b

            def multiply(self, a, b):
                return a * b

            def subtract(self, a, b):
                return a - b

        # Call methods
        calculator = CalculatorService()
        result1 = calculator.add(10, 5)
        result2 = calculator.multiply(3, 4)

        values = app.get([result1, result2])
        assert values == [15, 12], f"Expected [15, 12], got {values}"


def test_app_defaults_function(check_package_config, check_flmrun_app):
    """Test Case 18: Test default parameters for a function service."""
    with initialized_app("test-app-defaults-func"):

        @app.service()
        def sum_service(left, right):
            return left + right

        # Verify it works (defaults should be applied automatically)
        result = sum_service(100, 200)
        value = result.get()
        assert value == 300, f"Expected 300, got {value}"


def test_app_defaults_class(check_package_config, check_flmrun_app):
    """Test Case 19: Test default parameters for a class service."""
    with initialized_app("test-app-defaults-class"):

        @app.service()
        class CalculatorService:
            def add(self, a, b):
                return a + b

            def multiply(self, a, b):
                return a * b

            def subtract(self, a, b):
                return a - b

        result = CalculatorService().add(10, 1)

        value = result.get()
        assert value == 11, f"Expected 11, got {value}"


def test_app_defaults_instance(check_package_config, check_flmrun_app):
    """Test Case 20: Test fixed-instance parameters for a class service."""
    with initialized_app("test-app-defaults-instance"):

        @app.service(autoscale=False, warmup=1)
        class CounterService:
            def __init__(self):
                self.count = 0

            def increment(self):
                self.count += 1
                return self.count

            def get_count(self):
                return self.count

            def add(self, value):
                self.count += value
                return self.count

        counter_service = CounterService()

        counter_service.add(5).wait()
        counter_service.increment().wait()
        result = counter_service.get_count()

        value = result.get()
        assert value == 6, f"Expected 6, got {value}"


def test_app_auto_start(check_package_config, check_flmrun_app):
    """Test Case 21: Test init starts the process-wide app."""
    app.init("test-app-auto-start")
    try:
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-app-auto-start" in app_names, f"test-app-auto-start not found in applications: {app_names}"

        @app.service()
        def sum_service(left, right):

            return left + right

        result = sum_service(10, 20)
        value = result.get()
        assert value == 30, f"Expected 30, got {value}"
    finally:
        app.destroy()

    wait_for_application_deleted("test-app-auto-start")

    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-app-auto-start" not in app_names, f"test-app-auto-start should be unregistered but found in: {app_names}"


def test_app_explicit_destroy(check_package_config, check_flmrun_app):
    """Test Case 22: Test explicit destroy()."""
    app.init("test-app-explicit-destroy")
    try:
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-app-explicit-destroy" in app_names

        @app.service()
        def sum_service(left, right):
            return left + right

        result = sum_service(5, 7)
        value = result.get()
        assert value == 12, f"Expected 12, got {value}"
    finally:
        app.destroy()

    wait_for_application_deleted("test-app-explicit-destroy")

    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-app-explicit-destroy" not in app_names


def test_app_rejects_different_active_app(check_package_config, check_flmrun_app):
    """Test Case 23: A different app requires destroying the active app first."""
    app.init("test-app-active")
    try:
        with pytest.raises(flamepy.FlameError) as exc_info:
            app.init("test-app-other")

        assert exc_info.value.code == flamepy.FlameErrorCode.INVALID_STATE
    finally:
        app.destroy()


def test_app_repeated_init_reuses_active_app(check_package_config, check_flmrun_app):
    """Test Case 24: Repeating init for the active app is a no-op."""
    app.init("test-app-repeated-init")
    try:
        app.init("test-app-repeated-init")

        @app.service()
        def sum_service(left, right):

            return left + right

        result = sum_service(3, 4)
        value = result.get()
        assert value == 7, f"Expected 7, got {value}"

        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-app-repeated-init" in app_names
    finally:
        app.destroy()


def test_app_destroy_idempotent(check_package_config, check_flmrun_app):
    """Test Case 25: Test that calling destroy() multiple times is safe."""
    app.init("test-app-destroy-idempotent")
    try:

        @app.service()
        def sum_service(left, right):
            return left + right

        result = sum_service(1, 1)
        assert result.get() == 2
    finally:
        app.destroy()

    app.destroy()
    app.destroy()


# Generated service session IDs are covered by the App service tests above.

# =============================================================================
# Recursive App Tests (open_session)
# =============================================================================


def test_app_recursive_same_session(check_package_config, check_flmrun_app):
    """Test recursive app execution within the same session.

    This test verifies that a task can create another ServiceInstance using the same
    session ID, enabling recursive task submission within the same session.
    The open_session API allows this by returning the existing session instead of
    creating a new one.

    The outer application manages the lifecycle, while recursive declarations
    reuse the existing application registration and session context.
    """
    import logging
    import time
    import uuid

    logging.basicConfig(level=logging.INFO)
    logger = logging.getLogger(__name__)

    # The outer service owns its generated session. Recursive child proxies
    # borrow the executor-bound context for that same session.
    recursive_suffix = uuid.uuid4().hex[:8]
    shared_app_name = f"test-app-recursive-{recursive_suffix}"

    logger.info(f"[TEST] Starting recursive test: app={shared_app_name}")

    with initialized_app(shared_app_name):

        @app.service(autoscale=True)
        class RecursiveTestService(RecursiveService):
            pass

        # This test waits synchronously for nested results, so autoscaling
        # provides executor capacity for the child tasks.
        service = RecursiveTestService()
        logger.info(f"[TEST] Service created, session_id={service._session.id}")

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

        traced_value, session_ids = service.compute_recursive(2, True).get()
        assert traced_value == 4
        assert session_ids == [service._session.id] * 3

    wait_for_application_deleted(shared_app_name)
    app_names = [registered.name for registered in flamepy.list_applications()]
    assert shared_app_name not in app_names


# =============================================================================
# Flmrun Application Tests (from test_flmrun.py)
# =============================================================================


@pytest.fixture
def setup_flmrun_with_e2e():
    """
    Fixture to register a flmrun application with e2e modules available.

    This registers a custom flmrun application with PYTHONPATH set to include
    the e2e package, making e2e modules available to the app.
    """
    import os

    if not os.path.exists("/opt/e2e"):
        pytest.skip("Requires /opt/e2e directory (Docker E2E environment only)")

    flmrun = flamepy.get_application("flmrun")
    app_name = f"flmrun-e2e-{uuid.uuid4().hex[:8]}"

    flamepy.register_application(
        app_name,
        flamepy.ApplicationAttributes(
            working_directory="/opt/e2e",
            command=flmrun.command,
            arguments=flmrun.arguments,
            environments={"PYTHONPATH": "/opt/e2e/src"},
            installer="python",
            description="Flmrun with e2e modules available",
        ),
    )

    yield app_name

    flamepy.unregister_application(app_name)


@pytest.mark.skipif(not os.path.exists("/opt/e2e"), reason="Requires Docker E2E environment")
class TestFlmrunApplication:
    """Tests for flmrun application functionality."""

    def test_flmrun_application_registered(self, setup_flmrun_with_e2e):
        """Test that the custom flmrun application uses the template launcher."""
        app_name = setup_flmrun_with_e2e
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert app_name in app_names, f"{app_name} not found in applications: {app_names}"

        flmrun = flamepy.get_application(app_name)
        assert flmrun.name == app_name
        assert flmrun.state == flamepy.ApplicationState.ENABLED
        assert flmrun.command.endswith("/bin/uv")
        assert flmrun.arguments[:2] == ["run", "--python"]
        assert flmrun.arguments[-3:] == ["python", "-m", "flamepy.app.runpy"]

    def test_flmrun_sum_function(self, setup_flmrun_with_e2e):
        """Test Case 1: Run a simple sum function remotely."""
        from e2e.helpers import serialize_app_service_request, serialize_service_context

        app_name = setup_flmrun_with_e2e
        ctx = app.ServiceContext(execution_object=sum_func)
        common_data_bytes = serialize_service_context(ctx, app_name)
        ssn = flamepy.create_session(app_name, common_data_bytes)

        try:
            req = app.ServiceRequest(method=None, args=(1, 2))
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)

            result_ref = flamepy.core.ObjectRef.decode(result_bytes)
            result = flamepy.core.get_object(result_ref)

            assert result == 3, f"Expected 3, got {result}"
        finally:
            ssn.close()

    def test_flmrun_class_method(self, setup_flmrun_with_e2e):
        """Test Case 2: Run methods on a class instance."""
        from e2e.helpers import serialize_app_service_request, serialize_service_context

        app_name = setup_flmrun_with_e2e
        ctx = app.ServiceContext(execution_object=Calculator, constructor_args=())
        common_data_bytes = serialize_service_context(ctx, app_name)
        ssn = flamepy.create_session(app_name, common_data_bytes)

        try:
            req = app.ServiceRequest(method="add", args=(5, 3))
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 8, f"Expected 8, got {result}"

            req = app.ServiceRequest(method="multiply", args=(4, 7))
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 28, f"Expected 28, got {result}"

            req = app.ServiceRequest(method="subtract", args=(10, 3))
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 7, f"Expected 7, got {result}"
        finally:
            ssn.close()

    def test_flmrun_kwargs(self, setup_flmrun_with_e2e):
        """Test Case 3: Run a function with keyword arguments."""
        from e2e.helpers import serialize_app_service_request, serialize_service_context

        app_name = setup_flmrun_with_e2e
        ctx = app.ServiceContext(execution_object=greet_func)
        common_data_bytes = serialize_service_context(ctx, app_name)
        ssn = flamepy.create_session(app_name, common_data_bytes)

        try:
            req = app.ServiceRequest(method=None, kwargs={"name": "World", "greeting": "Hi"})
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == "Hi, World!", f"Expected 'Hi, World!', got {result}"

            req = app.ServiceRequest(method=None, kwargs={"name": "Python"})
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == "Hello, Python!", f"Expected 'Hello, Python!', got {result}"
        finally:
            ssn.close()

    def test_flmrun_retained_class_object(self, setup_flmrun_with_e2e):
        """Test Case 6: Retain a constructed class object across calls."""
        from e2e.helpers import serialize_app_service_request, serialize_service_context

        app_name = setup_flmrun_with_e2e
        ctx = app.ServiceContext(
            execution_object=Counter,
            constructor_args=(10,),
        )
        common_data_bytes = serialize_service_context(ctx, app_name)
        ssn = flamepy.create_session(app_name, common_data_bytes)

        try:
            req = app.ServiceRequest(method="increment")
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 11, f"Expected 11, got {result}"

            req = app.ServiceRequest(method="increment")
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 12, f"Expected 12, got {result}"

            req = app.ServiceRequest(method="add", args=(5,))
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 17, f"Expected 17, got {result}"

            req = app.ServiceRequest(method="get_count")
            req_bytes = serialize_app_service_request(req)
            result_bytes = ssn.invoke(req_bytes)
            result = flamepy.core.get_object(flamepy.core.ObjectRef.decode(result_bytes))
            assert result == 17, f"Expected 17, got {result}"
        finally:
            ssn.close()


# =============================================================================
# get_data Helper Tests (from test_get_data.py)
# =============================================================================


class TestGetData:
    """Tests for the `get_data` helper function in app."""

    def test_get_data_task_input_positional_args(self, check_package_config, check_flmrun_app):
        """TC-GD-001: Test get_data retrieves task input with positional arguments."""
        from flamepy.core import get_session

        with initialized_app("test-get-data-input-pos"):

            @app.service()
            def sum_service(left, right):
                return left + right

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

        with initialized_app("test-get-data-output"):

            @app.service()
            def multiply_service(left, right):
                return left * right

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
        invalid_data = b"this is not valid objectref data"

        with pytest.raises(DataError) as exc_info:
            get_data(invalid_data)

        assert exc_info.value.error_type == DataErrorType.DECODE_ERROR
        assert "decode" in str(exc_info.value).lower() or "failed" in str(exc_info.value).lower()

    def test_get_data_empty_bytes(self, check_package_config, check_flmrun_app):
        """TC-GD-008: Test get_data handles empty bytes gracefully."""
        empty_data = b""

        with pytest.raises(DataError) as exc_info:
            get_data(empty_data)

        assert exc_info.value.error_type == DataErrorType.DECODE_ERROR

    def test_get_data_class_method_input(self, check_package_config, check_flmrun_app):
        """TC-GD-006: Test get_data retrieves class method invocation input."""
        from flamepy.core import get_session

        with initialized_app("test-get-data-method"):

            @app.service()
            class CalculatorService:
                def add(self, a, b):
                    return a + b

                def multiply(self, a, b):
                    return a * b

                def subtract(self, a, b):
                    return a - b

            calc_service = CalculatorService()

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
        with initialized_app("test-drf-parallel-basic"):

            @app.service()
            def sum_service(left, right):
                return left + right

            results = [
                sum_service(1, 1),
                sum_service(2, 2),
                sum_service(3, 3),
                sum_service(4, 4),
                sum_service(5, 5),
            ]

            values = app.get(results)
            assert values == [2, 4, 6, 8, 10], f"Expected [2, 4, 6, 8, 10], got {values}"

    def test_parallel_tasks_high_concurrency(self, check_package_config, check_flmrun_app):
        """Test high concurrency with many parallel tasks."""
        with initialized_app("test-drf-parallel-high"):

            @app.service()
            def sum_service(left, right):
                return left + right

            num_tasks = 50
            results = [sum_service(i, i) for i in range(num_tasks)]
            values = app.get(results)

            expected = [i * 2 for i in range(num_tasks)]
            assert values == expected, "High concurrency test failed"

    def test_parallel_tasks_different_services(self, check_package_config, check_flmrun_app):
        """Test parallel execution across different services."""
        with initialized_app("test-drf-parallel-multi-svc"):

            @app.service()
            def sum_service(left, right):
                return left + right

            @app.service()
            class CalculatorService:
                def add(self, a, b):
                    return a + b

                def multiply(self, a, b):
                    return a * b

                def subtract(self, a, b):
                    return a - b

            calc_service = CalculatorService()

            results = [
                sum_service(10, 5),
                calc_service.multiply(3, 4),
                sum_service(20, 10),
                calc_service.subtract(15, 5),
            ]

            values = app.get(results)
            assert values == [15, 12, 30, 10], f"Expected [15, 12, 30, 10], got {values}"

    def test_parallel_select_iterator(self, check_package_config, check_flmrun_app):
        """Test using select() iterator for parallel task results."""
        with initialized_app("test-drf-parallel-select"):

            @app.service()
            def sum_service(left, right):
                return left + right

            results = [
                sum_service(1, 2),
                sum_service(3, 4),
                sum_service(5, 6),
            ]

            completed_values = []
            for result in app.select(results):
                completed_values.append(result.get())

            assert sorted(completed_values) == [3, 7, 11]


class TestTaskChaining:
    """Tests for task chaining and dependency patterns."""

    def test_task_chaining_sequential(self, check_package_config, check_flmrun_app):
        """Test sequential task chaining where output of one task feeds into next."""
        with initialized_app("test-drf-chain-seq"):

            @app.service()
            class CounterService:
                def __init__(self):
                    self.count = 0

                def increment(self):
                    self.count += 1
                    return self.count

                def get_count(self):
                    return self.count

                def add(self, value):
                    self.count += value
                    return self.count

            cnt_service = CounterService()

            cnt_service.add(10).wait()
            cnt_service.add(5).wait()
            cnt_service.increment().wait()
            result = cnt_service.get_count()

            value = result.get()
            assert value == 16, f"Expected 16, got {value}"

    def test_task_chaining_with_objectfuture(self, check_package_config, check_flmrun_app):
        """Test chaining using ObjectFuture as argument to next task."""
        with initialized_app("test-drf-chain-objfuture"):

            @app.service()
            class CounterService:
                def __init__(self):
                    self.count = 0

                def increment(self):
                    self.count += 1
                    return self.count

                def get_count(self):
                    return self.count

                def add(self, value):
                    self.count += value
                    return self.count

            cnt_service = CounterService()

            cnt_service.add(10).wait()
            intermediate = cnt_service.get_count()

            cnt_service.add(intermediate).wait()
            final = cnt_service.get_count()

            value = final.get()
            assert value == 20, f"Expected 20, got {value}"

    def test_task_dependency_graph(self, check_package_config, check_flmrun_app):
        """Test dependency graph: a(1,2)=3, b(3,4)=7, c(a,b)=10."""
        with initialized_app("test-drf-chain-graph"):

            @app.service()
            def sum_service(left, right):
                return left + right

            a = sum_service(1, 2)
            b = sum_service(3, 4)

            val_a, val_b = app.get([a, b])
            assert val_a == 3
            assert val_b == 7

            c = sum_service(val_a, val_b)
            val_c = c.get()
            assert val_c == 10, f"Expected 10, got {val_c}"


class TestMapReducePattern:
    """Tests for map-reduce distributed computing patterns."""

    def test_map_phase(self, check_package_config, check_flmrun_app):
        """Test map phase - apply same operation to multiple inputs."""
        with initialized_app("test-drf-map"):

            @app.service()
            class CalculatorService:
                def add(self, a, b):
                    return a + b

                def multiply(self, a, b):
                    return a * b

                def subtract(self, a, b):
                    return a - b

            calc_service = CalculatorService()

            inputs = [2, 3, 4, 5, 6]
            mapped_results = [calc_service.multiply(x, x) for x in inputs]

            values = app.get(mapped_results)
            assert values == [4, 9, 16, 25, 36], f"Map phase failed: {values}"

    def test_reduce_phase(self, check_package_config, check_flmrun_app):
        """Test reduce phase - aggregate multiple values pairwise."""
        with initialized_app("test-drf-reduce"):

            @app.service()
            def sum_service(left, right):
                return left + right

            values = [10, 20, 30, 40]

            level1 = [
                sum_service(values[0], values[1]),
                sum_service(values[2], values[3]),
            ]
            level1_values = app.get(level1)
            assert level1_values == [30, 70]

            result = sum_service(level1_values[0], level1_values[1])
            final = result.get()
            assert final == 100, f"Reduce phase failed: {final}"

    def test_full_map_reduce(self, check_package_config, check_flmrun_app):
        """Test map-reduce: square numbers [1,2,3,4] then sum = 1+4+9+16 = 30."""
        with initialized_app("test-drf-mapreduce"):

            @app.service()
            class CalculatorService:
                def add(self, a, b):
                    return a + b

                def multiply(self, a, b):
                    return a * b

                def subtract(self, a, b):
                    return a - b

            calc_service = CalculatorService()

            @app.service()
            def sum_service(left, right):
                return left + right

            inputs = [1, 2, 3, 4]

            mapped = [calc_service.multiply(x, x) for x in inputs]
            squared = app.get(mapped)
            assert squared == [1, 4, 9, 16], f"Map failed: {squared}"

            level1 = [
                sum_service(squared[0], squared[1]),
                sum_service(squared[2], squared[3]),
            ]
            level1_values = app.get(level1)

            final = sum_service(level1_values[0], level1_values[1])
            result = final.get()
            assert result == 30, f"MapReduce result should be 30, got {result}"


class TestDRFErrorHandling:
    """Tests for error handling in distributed execution."""

    def test_error_in_single_task(self, check_package_config, check_flmrun_app):
        """Test that errors in a single task are properly propagated."""

        with initialized_app("test-drf-error-single"):

            @app.service()
            def failing_func(x: int) -> int:
                if x < 0:
                    raise ValueError(f"Negative value not allowed: {x}")
                return x * 2

            result = failing_func(5)
            assert result.get() == 10

            error_result = failing_func(-1)
            with pytest.raises(Exception):
                error_result.get()

    def test_partial_failure_in_parallel(self, check_package_config, check_flmrun_app):
        """Test handling when some tasks fail in parallel execution."""

        with initialized_app("test-drf-error-partial"):

            @app.service()
            def conditional_fail(x: int) -> int:
                if x == 3:
                    raise ValueError("Task 3 always fails")
                return x * 10

            results = [
                conditional_fail(1),
                conditional_fail(2),
                conditional_fail(3),
                conditional_fail(4),
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


class TestDRFRetainedClassServices:
    """Tests for retained class-service behavior in DRF."""

    def test_retained_counter_operations(self, check_package_config, check_flmrun_app):
        """Test a retained counter object across multiple operations."""
        with initialized_app("test-drf-retained-counter"):

            @app.service()
            class CounterService:
                def __init__(self):
                    self.count = 0

                def increment(self):
                    self.count += 1
                    return self.count

                def get_count(self):
                    return self.count

                def add(self, value):
                    self.count += value
                    return self.count

            cnt_service = CounterService()

            cnt_service.add(100).wait()
            cnt_service.increment().wait()
            cnt_service.increment().wait()
            cnt_service.add(50).wait()

            result = cnt_service.get_count()
            value = result.get()
            assert value == 152, f"Expected 152, got {value}"

    def test_retained_object_isolation_between_services(self, check_package_config, check_flmrun_app):
        """Test that different class handles retain separate objects."""
        with initialized_app("test-drf-retained-isolation"):

            @app.service(autoscale=False, warmup=1)
            class CounterService:
                def __init__(self):
                    self.count = 0

                def increment(self):
                    self.count += 1
                    return self.count

                def get_count(self):
                    return self.count

                def add(self, value):
                    self.count += value
                    return self.count

            svc1 = CounterService()
            svc2 = CounterService()

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
        """Test that session is properly cleaned up when App exits."""
        app_name = "test-drf-session-cleanup"

        with initialized_app(app_name):

            @app.service()
            def sum_service(left, right):
                return left + right

            result = sum_service(1, 2)
            assert result.get() == 3

            session_id = sum_service._session.id

        sessions = flamepy.list_sessions()
        session = next((s for s in sessions if s.id == session_id), None)
        if session:
            assert session.state == flamepy.SessionState.CLOSED

    def test_multiple_apps_same_app(self, check_package_config, check_flmrun_app):
        """Test repeated init shares the active application."""
        app_name = "test-drf-multi-app"

        with initialized_app(app_name):

            @app.service()
            def svc1(left, right):
                return left + right

            r1 = svc1(10, 20)
            val1 = r1.get()
            assert val1 == 30

            app.init(app_name)

            @app.service()
            def svc2(left, right):
                return left + right

            r2 = svc2(100, 200)
            val2 = r2.get()
            assert val2 == 300


class TestDRFPerformance:
    """Tests for performance characteristics in DRF."""

    def test_throughput_many_small_tasks(self, check_package_config, check_flmrun_app):
        """Test throughput with many small tasks."""
        import time as time_module

        with initialized_app("test-drf-throughput"):

            @app.service()
            def sum_service(left, right):
                return left + right

            start_time = time_module.time()

            num_tasks = 100
            results = [sum_service(i, 1) for i in range(num_tasks)]
            values = app.get(results)

            elapsed = time_module.time() - start_time

            expected = [i + 1 for i in range(num_tasks)]
            assert values == expected

            throughput = num_tasks / elapsed if elapsed > 0 else 0
            print(f"Throughput: {throughput:.2f} tasks/sec for {num_tasks} tasks in {elapsed:.2f}s")


class TestDRFEdgeCases:
    """Tests for edge cases and boundary conditions in DRF."""

    def test_empty_arguments(self, check_package_config, check_flmrun_app):
        """Test calling function with no arguments."""

        with initialized_app("test-drf-empty-args"):

            @app.service()
            def get_constant() -> int:
                return 42

            result = get_constant()
            value = result.get()
            assert value == 42

    def test_none_arguments(self, check_package_config, check_flmrun_app):
        """Test handling None as argument."""

        with initialized_app("test-drf-none-args"):

            @app.service()
            def handle_none(x) -> str:
                return "none" if x is None else "not-none"

            result = handle_none(None)
            value = result.get()
            assert value == "none"

    def test_large_return_value(self, check_package_config, check_flmrun_app):
        """Test handling large return values."""

        with initialized_app("test-drf-large-return"):

            @app.service()
            def create_large_list(n: int) -> list:
                return list(range(n))

            result = create_large_list(10000)
            value = result.get()
            assert len(value) == 10000
            assert value[0] == 0
            assert value[9999] == 9999

    def test_nested_data_structures(self, check_package_config, check_flmrun_app):
        """Test handling nested data structures."""

        with initialized_app("test-drf-nested-data"):

            @app.service()
            def process_nested(data: dict) -> dict:
                return {
                    "input": data,
                    "processed": True,
                    "nested": {"level": 2, "data": [1, 2, 3]},
                }

            input_data = {"key": "value", "list": [1, 2, 3]}
            result = process_nested(input_data)
            value = result.get()

            assert value["processed"] is True
            assert value["input"] == input_data
            assert value["nested"]["level"] == 2


class TestDRFConcurrentAccess:
    """Tests for concurrent access patterns in DRF."""

    def test_concurrent_service_calls(self, check_package_config, check_flmrun_app):
        """Test concurrent calls to the same service."""
        with initialized_app("test-drf-concurrent-calls"):

            @app.service()
            def sum_service(left, right):
                return left + right

            num_calls = 30
            results = [sum_service(i, i + 1) for i in range(num_calls)]

            values = app.get(results)
            expected = [i + (i + 1) for i in range(num_calls)]
            assert values == expected

    def test_interleaved_operations(self, check_package_config, check_flmrun_app):
        """Test interleaved operations on multiple services."""
        with initialized_app("test-drf-interleaved"):

            @app.service()
            def sum_service(left, right):
                return left + right

            @app.service()
            class CalculatorService:
                def add(self, a, b):
                    return a + b

                def multiply(self, a, b):
                    return a * b

                def subtract(self, a, b):
                    return a - b

            calc_service = CalculatorService()

            results = []
            for i in range(10):
                results.append(sum_service(i, 1))
                results.append(calc_service.multiply(i, 2))

            values = app.get(results)

            for i in range(10):
                sum_idx = i * 2
                mult_idx = i * 2 + 1
                assert values[sum_idx] == i + 1, f"Sum at {sum_idx} wrong"
                assert values[mult_idx] == i * 2, f"Multiply at {mult_idx} wrong"
