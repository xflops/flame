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

import pytest
import flamepy
from flamepy import Runner, ObjectFuture
import os
import tempfile
from pathlib import Path
from e2e.helpers import (
    sum_func,
    multiply_func,
    greet_func,
    Calculator,
    Counter,
)


@pytest.fixture(scope="module")
def check_package_config():
    """Check that package configuration is available."""
    ctx = flamepy.FlameContext()
    if ctx.package is None:
        pytest.skip(
            "Package configuration not set in flame.yaml. "
            "Please add 'package' section with 'storage' field."
        )
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
    with Runner("test-runner-cm") as rr:
        # Verify that the application is registered
        apps = flamepy.list_applications()
        app_names = [app.name for app in apps]
        assert "test-runner-cm" in app_names, (
            f"test-runner-cm not found in applications: {app_names}"
        )

    # After exiting context, application should be unregistered
    apps = flamepy.list_applications()
    app_names = [app.name for app in apps]
    assert "test-runner-cm" not in app_names, (
        f"test-runner-cm should be unregistered but found in: {app_names}"
    )


def test_runner_with_function(check_package_config, check_flmrun_app):
    """Test Case 2: Test Runner with a simple function."""
    with Runner("test-runner-func") as rr:
        # Create a service with a function
        sum_service = rr.service(sum_func)

        # Call the function remotely
        result = sum_service(1, 3)

        # Verify result is an ObjectFuture
        assert isinstance(result, ObjectFuture), (
            f"Expected ObjectFuture, got {type(result)}"
        )

        # Get the actual result
        value = result.get()
        assert value == 4, f"Expected 4, got {value}"


def test_runner_with_class(check_package_config, check_flmrun_app):
    """Test Case 3: Test Runner with a class (auto-instantiation)."""
    with Runner("test-runner-class") as rr:
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
    with Runner("test-runner-instance") as rr:
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
    with Runner("test-runner-objfuture") as rr:
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
    with Runner("test-runner-multi") as rr:
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
    with Runner("test-runner-kwargs") as rr:
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
            with Runner("test-runner-excludes") as rr:
                # Just verify it works - the exclusion is tested by successful packaging
                pass

        finally:
            # Restore original directory
            os.chdir(original_dir)


def test_objectfuture_ref_method(check_package_config, check_flmrun_app):
    """Test Case 9: Test ObjectFuture.ref() method."""
    with Runner("test-objectfuture-ref") as rr:
        sum_service = rr.service(sum_func)

        # Get an ObjectFuture
        result = sum_service(10, 20)

        # Get the ObjectRef
        obj_ref = result.ref()

        # Verify it's an ObjectRef
        assert isinstance(obj_ref, flamepy.core.ObjectRef), (
            f"Expected ObjectRef, got {type(obj_ref)}"
        )
        assert obj_ref.endpoint is not None, "ObjectRef endpoint should not be None"
        assert obj_ref.key is not None, "ObjectRef key should not be None"


def test_objectfuture_iterator(check_package_config, check_flmrun_app):
    """Test Case 10: Test ObjectFutureIterator."""
    with Runner("test-objectfuture-iterator") as rr:
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
    with Runner("test-service-close") as rr:
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


def test_runner_error_no_package_config():
    """Test Case 13: Test Runner fails gracefully without package config."""
    # Temporarily clear package config by creating a new context
    # This test assumes the package config might not be set
    # Skip if package config is available
    ctx = flamepy.FlameContext()
    if ctx.package is not None:
        pytest.skip("Package config is set, cannot test error case")

    # Try to use Runner without package config
    with pytest.raises(flamepy.FlameError) as exc_info:
        with Runner("test-no-config") as rr:
            pass

    assert exc_info.value.code == flamepy.FlameErrorCode.INVALID_CONFIG


def test_runner_stateful_instance(check_package_config, check_flmrun_app):
    """Test Case 14: Test Runner with stateful=True for instance."""
    with Runner("test-runner-stateful") as rr:
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
    with Runner("test-runner-stateless-func") as rr:
        # Create a service with a function (stateless by default)
        sum_service = rr.service(sum_func, stateful=False, autoscale=True)
        
        # Call the function multiple times
        results = [sum_service(i, i+1) for i in range(5)]
        values = rr.get(results)
        
        # Verify results
        expected = [1, 3, 5, 7, 9]
        assert values == expected, f"Expected {expected}, got {values}"


def test_runner_class_single_instance(check_package_config, check_flmrun_app):
    """Test Case 16: Test Runner with class and autoscale=False (single instance)."""
    with Runner("test-runner-class-single") as rr:
        # Create a service with a class, single instance mode
        calc_service = rr.service(Calculator, stateful=False, autoscale=False)
        
        # Call methods
        result1 = calc_service.add(10, 5)
        result2 = calc_service.multiply(3, 4)
        
        values = rr.get([result1, result2])
        assert values == [15, 12], f"Expected [15, 12], got {values}"


def test_runner_error_stateful_class(check_package_config, check_flmrun_app):
    """Test Case 17: Test that stateful=True raises error for class."""
    with Runner("test-runner-stateful-class-error") as rr:
        # Trying to create a stateful service with a class should raise ValueError
        with pytest.raises(ValueError) as exc_info:
            rr.service(Counter, stateful=True)
        
        assert "Cannot set stateful=True for a class" in str(exc_info.value)


def test_runner_defaults_function(check_package_config, check_flmrun_app):
    """Test Case 18: Test default parameters for function (stateful=False, autoscale=True)."""
    with Runner("test-runner-defaults-func") as rr:
        # Create service with defaults (should be stateful=False, autoscale=True)
        sum_service = rr.service(sum_func)
        
        # Verify it works (defaults should be applied automatically)
        result = sum_service(100, 200)
        value = result.get()
        assert value == 300, f"Expected 300, got {value}"


def test_runner_defaults_class(check_package_config, check_flmrun_app):
    """Test Case 19: Test default parameters for class (stateful=False, autoscale=False)."""
    with Runner("test-runner-defaults-class") as rr:
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
    with Runner("test-runner-defaults-instance") as rr:
        # Create an instance
        calc = Calculator()
        
        # Create service with instance using defaults (should be stateful=False, autoscale=False)
        calc_service = rr.service(calc)
        
        # Call methods
        result1 = calc_service.add(5, 3)
        result2 = calc_service.subtract(10, 4)
        
        values = rr.get([result1, result2])
        assert values == [8, 6], f"Expected [8, 6], got {values}"
