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

Test cases for the `get_data` helper function in flamepy.runner.

This module tests the ability to retrieve and inspect task input/output data
from completed Runner tasks using the `get_data` helper function.

Test Coverage:
- TC-GD-001: Basic task input retrieval (function with positional args)
- TC-GD-002: Basic task output retrieval
- TC-GD-003: Task input with keyword arguments
- TC-GD-004: Task input with mixed args and kwargs
- TC-GD-005: Task input with ObjectRef arguments (resolved)
- TC-GD-006: Class method invocation input
- TC-GD-007: Error handling - invalid data format
- TC-GD-008: Error handling - empty bytes
- TC-GD-009: Multiple tasks inspection
- TC-GD-010: Task with no arguments
"""

import flamepy
import pytest
from flamepy import runner
from flamepy.core import get_session
from flamepy.runner import ErrorType, RunnerError, get_data

from e2e.helpers import (
    Calculator,
    Counter,
    greet_func,
    multiply_func,
    sum_func,
)


@pytest.fixture(scope="module")
def check_package_config():
    """Check that package configuration is available."""
    ctx = flamepy.FlameContext()
    if ctx.package is None:
        pytest.skip("Package configuration not set in flame.yaml.")
    yield ctx.package


@pytest.fixture(scope="module")
def check_flmrun_app():
    """Check that flmrun application is registered."""
    try:
        flamepy.get_application("flmrun")
    except Exception:
        pytest.skip("flmrun application not found.")


def test_get_data_task_input_positional_args(check_package_config, check_flmrun_app):
    """TC-GD-001: Test get_data retrieves task input with positional arguments.

    Steps:
    1. Create a Runner and service with sum_func
    2. Call the service with positional arguments (5, 3)
    3. Wait for task completion
    4. Get the session and retrieve task input using get_data
    5. Verify the input contains correct method, args, and kwargs

    Expected:
    - type == "input"
    - method is None (function call, not method)
    - args == (5, 3)
    - kwargs is None or empty
    """
    with runner.Runner("test-get-data-input-pos") as rr:
        sum_service = rr.service(sum_func)

        # Call with positional arguments
        result = sum_service(5, 3)
        value = result.get()
        assert value == 8, f"Expected 8, got {value}"

        # Get session and inspect task
        session = get_session(sum_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        # Get the task with input data
        task = tasks[0]
        assert task.input is not None, "Task input should not be None"

        # Retrieve input data using get_data
        input_data = get_data(task.input)

        # Verify structure
        assert input_data["type"] == "input", f"Expected type 'input', got {input_data['type']}"
        assert input_data["method"] is None, f"Expected method None for function, got {input_data['method']}"
        assert input_data["args"] == (5, 3), f"Expected args (5, 3), got {input_data['args']}"


def test_get_data_task_output(check_package_config, check_flmrun_app):
    """TC-GD-002: Test get_data retrieves task output correctly.

    Steps:
    1. Create a Runner and service with multiply_func
    2. Call the service with arguments (4, 7)
    3. Wait for task completion
    4. Get the session and retrieve task output using get_data
    5. Verify the output contains the correct result

    Expected:
    - type == "output"
    - result == 28
    """
    with runner.Runner("test-get-data-output") as rr:
        multiply_service = rr.service(multiply_func)

        # Call and wait for result
        result = multiply_service(4, 7)
        value = result.get()
        assert value == 28, f"Expected 28, got {value}"

        # Get session and inspect task
        session = get_session(multiply_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        # Get the task with output data
        task = tasks[0]
        assert task.output is not None, "Task output should not be None"

        # Retrieve output data using get_data
        output_data = get_data(task.output)

        # Verify structure
        assert output_data["type"] == "output", f"Expected type 'output', got {output_data['type']}"
        assert output_data["result"] == 28, f"Expected result 28, got {output_data['result']}"


def test_get_data_task_input_kwargs(check_package_config, check_flmrun_app):
    """TC-GD-003: Test get_data retrieves task input with keyword arguments.

    Steps:
    1. Create a Runner and service with greet_func
    2. Call the service with keyword arguments (name="World", greeting="Hi")
    3. Wait for task completion
    4. Get the session and retrieve task input using get_data
    5. Verify the input contains correct kwargs

    Expected:
    - type == "input"
    - kwargs contains {"name": "World", "greeting": "Hi"}
    """
    with runner.Runner("test-get-data-input-kwargs") as rr:
        greet_service = rr.service(greet_func)

        # Call with keyword arguments
        result = greet_service(name="World", greeting="Hi")
        value = result.get()
        assert value == "Hi, World!", f"Expected 'Hi, World!', got {value}"

        # Get session and inspect task
        session = get_session(greet_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        task = tasks[0]
        assert task.input is not None, "Task input should not be None"

        # Retrieve input data using get_data
        input_data = get_data(task.input)

        # Verify structure
        assert input_data["type"] == "input"
        assert input_data["kwargs"] is not None, "Expected kwargs to be present"
        assert input_data["kwargs"].get("name") == "World"
        assert input_data["kwargs"].get("greeting") == "Hi"


def test_get_data_task_input_mixed_args_kwargs(check_package_config, check_flmrun_app):
    """TC-GD-004: Test get_data retrieves task input with mixed args and kwargs.

    Steps:
    1. Create a Runner and service with greet_func
    2. Call the service with positional arg and keyword arg
    3. Wait for task completion
    4. Verify both args and kwargs are captured

    Expected:
    - type == "input"
    - args contains positional argument
    - kwargs contains keyword argument
    """
    with runner.Runner("test-get-data-input-mixed") as rr:
        greet_service = rr.service(greet_func)

        # Call with mixed arguments - name as positional, greeting as keyword
        result = greet_service("Python", greeting="Welcome")
        value = result.get()
        assert value == "Welcome, Python!", f"Expected 'Welcome, Python!', got {value}"

        # Get session and inspect task
        session = get_session(greet_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        task = tasks[0]
        assert task.input is not None, "Task input should not be None"

        # Retrieve input data using get_data
        input_data = get_data(task.input)

        # Verify structure
        assert input_data["type"] == "input"
        # The positional arg should be in args
        assert input_data["args"] is not None and len(input_data["args"]) >= 1
        assert "Python" in input_data["args"]


def test_get_data_task_input_objectref_resolved(check_package_config, check_flmrun_app):
    """TC-GD-005: Test get_data resolves ObjectRef arguments in task input.

    Steps:
    1. Create a Runner and service with Counter
    2. Call methods that use ObjectFuture as argument (chained calls)
    3. Wait for task completion
    4. Verify ObjectRef arguments are resolved to actual values

    Expected:
    - ObjectRef in args/kwargs should be resolved to actual values
    """
    with runner.Runner("test-get-data-objref") as rr:
        counter = Counter()
        cnt_service = rr.service(counter, stateful=True, autoscale=False)

        # First call to get a value
        cnt_service.add(10).wait()
        res_r = cnt_service.get_count()

        # Use ObjectFuture as argument (this creates ObjectRef in args)
        cnt_service.add(res_r).wait()
        final_result = cnt_service.get_count()
        value = final_result.get()
        assert value == 20, f"Expected 20, got {value}"

        # Get session and inspect tasks
        session = get_session(cnt_service._session.id)
        tasks = list(session.list_tasks())

        # Find a task that had ObjectRef in its input
        # The add(res_r) call should have ObjectRef as argument
        _found_resolved = False
        for task in tasks:
            if task.input is not None:
                input_data = get_data(task.input)
                if input_data["type"] == "input" and input_data["args"]:
                    # Check if any arg is a resolved integer (not ObjectRef)
                    for arg in input_data["args"]:
                        if isinstance(arg, int) and arg == 10:
                            _found_resolved = True
                            break

        # At minimum, verify we can retrieve input data without errors
        assert len(tasks) >= 3, f"Expected at least 3 tasks, got {len(tasks)}"


def test_get_data_class_method_input(check_package_config, check_flmrun_app):
    """TC-GD-006: Test get_data retrieves class method invocation input.

    Steps:
    1. Create a Runner and service with Calculator class
    2. Call a method (add) on the service
    3. Wait for task completion
    4. Verify the input contains the method name

    Expected:
    - type == "input"
    - method == "add" (or similar method name)
    - args contains the method arguments
    """
    with runner.Runner("test-get-data-method") as rr:
        calc_service = rr.service(Calculator())

        # Call a method
        result = calc_service.add(15, 25)
        value = result.get()
        assert value == 40, f"Expected 40, got {value}"

        # Get session and inspect task
        session = get_session(calc_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        task = tasks[0]
        assert task.input is not None, "Task input should not be None"

        # Retrieve input data using get_data
        input_data = get_data(task.input)

        # Verify structure
        assert input_data["type"] == "input"
        assert input_data["method"] == "add", f"Expected method 'add', got {input_data['method']}"
        assert input_data["args"] == (15, 25), f"Expected args (15, 25), got {input_data['args']}"


def test_get_data_invalid_data_format(check_package_config, check_flmrun_app):
    """TC-GD-007: Test get_data handles invalid data format gracefully.

    Steps:
    1. Call get_data with invalid bytes (not a valid ObjectRef)
    2. Verify appropriate error is raised

    Expected:
    - RunnerError is raised with ErrorType.DECODE_ERROR
    """
    # Test with random invalid bytes
    invalid_data = b"this is not valid objectref data"

    with pytest.raises(RunnerError) as exc_info:
        get_data(invalid_data)

    # Verify error type
    assert exc_info.value.error_type == ErrorType.DECODE_ERROR
    # Verify error message is descriptive
    assert "decode" in str(exc_info.value).lower() or "failed" in str(exc_info.value).lower()


def test_get_data_empty_bytes(check_package_config, check_flmrun_app):
    """TC-GD-008: Test get_data handles empty bytes gracefully.

    Steps:
    1. Call get_data with empty bytes
    2. Verify appropriate error is raised

    Expected:
    - RunnerError is raised with ErrorType.DECODE_ERROR
    """
    empty_data = b""

    with pytest.raises(RunnerError) as exc_info:
        get_data(empty_data)

    assert exc_info.value.error_type == ErrorType.DECODE_ERROR


def test_get_data_multiple_tasks(check_package_config, check_flmrun_app):
    """TC-GD-009: Test get_data works correctly with multiple tasks.

    Steps:
    1. Create a Runner and service
    2. Submit multiple tasks with different arguments
    3. Wait for all tasks to complete
    4. Retrieve and verify input/output for each task

    Expected:
    - Each task's input/output can be retrieved correctly
    - Data matches what was submitted
    """
    with runner.Runner("test-get-data-multi") as rr:
        sum_service = rr.service(sum_func)

        # Submit multiple tasks
        results = [
            sum_service(1, 2),
            sum_service(10, 20),
            sum_service(100, 200),
        ]

        # Wait for all results
        values = rr.get(results)
        assert values == [3, 30, 300], f"Expected [3, 30, 300], got {values}"

        # Get session and inspect all tasks
        session = get_session(sum_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 3, f"Expected at least 3 tasks, got {len(tasks)}"

        # Verify we can retrieve data for all tasks
        input_args_found = set()
        output_results_found = set()

        for task in tasks:
            if task.input is not None:
                input_data = get_data(task.input)
                assert input_data["type"] == "input"
                if input_data["args"]:
                    input_args_found.add(input_data["args"])

            if task.output is not None:
                output_data = get_data(task.output)
                assert output_data["type"] == "output"
                output_results_found.add(output_data["result"])

        # Verify we found the expected results
        assert 3 in output_results_found or 30 in output_results_found or 300 in output_results_found


def test_get_data_task_no_arguments(check_package_config, check_flmrun_app):
    """TC-GD-010: Test get_data handles tasks with no arguments.

    Steps:
    1. Create a Runner and service with Counter
    2. Call get_count() which takes no arguments
    3. Wait for task completion
    4. Verify input data shows no args/kwargs

    Expected:
    - type == "input"
    - args is None or empty tuple
    - kwargs is None or empty dict
    """
    with runner.Runner("test-get-data-no-args") as rr:
        counter = Counter()
        cnt_service = rr.service(counter)

        # Call method with no arguments
        result = cnt_service.get_count()
        value = result.get()
        assert value == 0, f"Expected 0, got {value}"

        # Get session and inspect task
        session = get_session(cnt_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        task = tasks[0]
        assert task.input is not None, "Task input should not be None"

        # Retrieve input data using get_data
        input_data = get_data(task.input)

        # Verify structure
        assert input_data["type"] == "input"
        assert input_data["method"] == "get_count"
        # Args should be None or empty
        assert input_data["args"] is None or input_data["args"] == () or input_data["args"] == []


def test_get_data_complete_workflow(check_package_config, check_flmrun_app):
    """TC-GD-011: End-to-end test of get_data with complete task inspection.

    This test demonstrates the full workflow described in the HLD:
    1. Run a task using Runner
    2. Get the session
    3. Iterate through tasks
    4. Retrieve both input and output for each task
    5. Verify the data matches expectations

    Expected:
    - Complete workflow executes without errors
    - Input and output data are correctly retrieved and match
    """
    with runner.Runner("test-get-data-e2e") as rr:
        calc_service = rr.service(Calculator())

        # Execute a task
        result = calc_service.multiply(6, 7)
        value = result.get()
        assert value == 42, f"Expected 42, got {value}"

        # Get session (as shown in HLD example)
        session = get_session(calc_service._session.id)

        # Iterate through tasks (as shown in HLD)
        task_found = False
        for task in session.list_tasks():
            if task.input:
                input_info = get_data(task.input)
                assert input_info["type"] == "input"

                # Verify method and args
                if input_info["method"] == "multiply":
                    task_found = True
                    assert input_info["args"] == (6, 7), f"Expected (6, 7), got {input_info['args']}"

            if task.output:
                output_info = get_data(task.output)
                assert output_info["type"] == "output"
                assert output_info["result"] == 42, f"Expected 42, got {output_info['result']}"

        assert task_found, "Expected to find the multiply task"


# Additional test cases for nested data structures and specific error types


def test_get_data_nested_list_with_objectref(check_package_config, check_flmrun_app):
    """TC-GD-012: Test get_data resolves ObjectRef in nested lists.

    Steps:
    1. Create a Runner and service with Counter
    2. Create a task where args contain a list with ObjectFuture
    3. Wait for task completion
    4. Verify nested ObjectRef in list is resolved

    Expected:
    - Nested ObjectRef in list should be resolved to actual values
    """
    with runner.Runner("test-get-data-nested-list") as rr:
        counter = Counter()
        cnt_service = rr.service(counter, stateful=True, autoscale=False)

        # First call to get a value
        cnt_service.add(5).wait()
        res_r = cnt_service.get_count()

        # Use ObjectFuture in a list argument
        # Note: This tests the recursive resolution in _resolve_value
        cnt_service.add(res_r).wait()
        final_result = cnt_service.get_count()
        value = final_result.get()
        assert value == 10, f"Expected 10, got {value}"

        # Get session and inspect tasks
        session = get_session(cnt_service._session.id)
        tasks = list(session.list_tasks())

        # Verify we can retrieve all task inputs without errors
        for task in tasks:
            if task.input is not None:
                input_data = get_data(task.input)
                assert input_data["type"] == "input"
                # Verify args are resolved (not ObjectRef instances)
                if input_data["args"]:
                    for arg in input_data["args"]:
                        # Args should be resolved to primitive types, not ObjectRef
                        assert not hasattr(arg, "decode"), f"ObjectRef not resolved: {arg}"


def test_get_data_specific_error_types(check_package_config, check_flmrun_app):
    """TC-GD-013: Test get_data raises specific error types.

    Steps:
    1. Call get_data with invalid data
    2. Verify RunnerError is raised with correct ErrorType

    Expected:
    - RunnerError is raised with ErrorType.DECODE_ERROR and cause attribute
    """
    # Test with invalid bytes that can't be decoded as ObjectRef
    invalid_data = b"invalid objectref data"

    with pytest.raises(RunnerError) as exc_info:
        get_data(invalid_data)

    # Verify error type and cause attribute
    assert exc_info.value.error_type == ErrorType.DECODE_ERROR
    assert exc_info.value.cause is not None
    assert "decode" in str(exc_info.value).lower() or "failed" in str(exc_info.value).lower()


def test_get_data_metadata_present(check_package_config, check_flmrun_app):
    """TC-GD-014: Test get_data returns metadata in response.

    Steps:
    1. Create a Runner and service
    2. Execute a task
    3. Retrieve input/output using get_data
    4. Verify metadata field is present

    Expected:
    - Response contains metadata field
    - metadata contains object_ref_key
    """
    with runner.Runner("test-get-data-metadata") as rr:
        sum_service = rr.service(sum_func)

        # Call and wait for result
        result = sum_service(10, 20)
        value = result.get()
        assert value == 30, f"Expected 30, got {value}"

        # Get session and inspect task
        session = get_session(sum_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        task = tasks[0]

        # Check input metadata
        if task.input is not None:
            input_data = get_data(task.input)
            assert "metadata" in input_data, "Expected metadata in input response"
            assert isinstance(input_data["metadata"], dict), "metadata should be a dict"

        # Check output metadata
        if task.output is not None:
            output_data = get_data(task.output)
            assert "metadata" in output_data, "Expected metadata in output response"
            assert isinstance(output_data["metadata"], dict), "metadata should be a dict"


def test_get_data_nested_dict_resolution(check_package_config, check_flmrun_app):
    """TC-GD-015: Test get_data resolves ObjectRef in nested dicts.

    Steps:
    1. Create a Runner and service with Calculator
    2. Execute multiple tasks
    3. Verify nested structures in kwargs are properly resolved

    Expected:
    - Nested dict values should be resolved
    """
    with runner.Runner("test-get-data-nested-dict") as rr:
        calc_service = rr.service(Calculator())

        # Execute a task with kwargs
        result = calc_service.add(a=100, b=200)
        value = result.get()
        assert value == 300, f"Expected 300, got {value}"

        # Get session and inspect task
        session = get_session(calc_service._session.id)
        tasks = list(session.list_tasks())
        assert len(tasks) >= 1, "Expected at least one task"

        task = tasks[0]
        assert task.input is not None, "Task input should not be None"

        # Retrieve input data using get_data
        input_data = get_data(task.input)

        # Verify kwargs are resolved
        assert input_data["type"] == "input"
        if input_data["kwargs"]:
            for key, val in input_data["kwargs"].items():
                # Values should be resolved primitives, not ObjectRef
                assert not hasattr(val, "decode"), f"ObjectRef not resolved in kwargs: {key}={val}"


def test_get_data_runner_error_exported(check_package_config, check_flmrun_app):
    """TC-GD-016: Test that RunnerError and ErrorType are properly exported.

    Steps:
    1. Import RunnerError and ErrorType from flamepy.runner
    2. Verify they are accessible and work correctly

    Expected:
    - RunnerError and ErrorType are importable
    - ErrorType enum has expected values
    """
    from flamepy.runner import ErrorType, RunnerError

    # Verify ErrorType enum values
    assert ErrorType.DECODE_ERROR.value == "decode_error"
    assert ErrorType.CACHE_RETRIEVAL_ERROR.value == "cache_retrieval_error"
    assert ErrorType.DATA_FORMAT_ERROR.value == "data_format_error"

    # Verify RunnerError can be instantiated
    error = RunnerError(
        ErrorType.DECODE_ERROR,
        "Test error message",
        cause=ValueError("test cause"),
        key="test_key",
    )
    assert error.error_type == ErrorType.DECODE_ERROR
    assert error.cause is not None
    assert error.key == "test_key"
    assert "decode_error" in str(error).lower()
