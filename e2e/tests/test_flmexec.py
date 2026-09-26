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

import json
import textwrap

import flamepy
import pytest

NODEPS_RESULT_PREFIX = "FLMEXEC_APP_NODEPS_RESULT="
NUMPY_RESULT_PREFIX = "FLMEXEC_APP_NUMPY_RESULT="


@pytest.fixture(scope="module")
def check_flmexec_app_environment():
    context = flamepy.FlameContext()
    package_config = getattr(context, "package", None)
    cache_config = getattr(context, "cache", None)
    has_package_storage = package_config is not None and getattr(package_config, "storage", None) is not None
    has_cache_endpoint = cache_config is not None
    if not has_package_storage and not has_cache_endpoint:
        pytest.skip("App package storage is not configured")

    try:
        if flamepy.get_application("flmexec") is None:
            pytest.skip("flmexec application is not registered")
        if flamepy.get_application("flmrun") is None:
            pytest.skip("flmrun application is not registered")
    except Exception as exc:
        pytest.skip(f"Flame cluster is not available: {exc}")


def _invoke_flmexec_python(script: str, runtime: str | None = None) -> str:
    session = flamepy.create_session("flmexec")
    try:
        request = {"language": "python", "runtime": runtime, "code": script, "input": None}
        raw_response = session.invoke(json.dumps(request).encode("utf-8"))
    finally:
        session.close()

    response = json.loads(raw_response.decode("utf-8"))
    return bytes(response["data"]).decode("utf-8")


@pytest.mark.timeout(600)
def test_flmexec_python_script_starts_app_without_project_metadata(check_flmexec_app_environment):
    script = textwrap.dedent(
        f"""
        import json
        import sys
        import traceback
        import uuid

        try:
            import flamepy.app as app

            app_name = f"test-flmexec-app-nodeps-{{uuid.uuid4().hex[:8]}}"

            app.init(app_name)
            try:
                @app.service()
                def service(value):
                    return value * value

                result = app.get([service.remote(10), service.remote(20)])
            finally:
                app.destroy()

            print("{NODEPS_RESULT_PREFIX}" + json.dumps(result))
        except BaseException:
            traceback.print_exc(file=sys.stdout)
            sys.stdout.flush()
            raise
        """
    )

    output = _invoke_flmexec_python(script)
    result_line = next((line for line in output.splitlines() if line.startswith(NODEPS_RESULT_PREFIX)), None)
    assert result_line is not None, f"missing result line in flmexec output:\n{output}"

    result = json.loads(result_line.removeprefix(NODEPS_RESULT_PREFIX))
    assert result == [100, 400]


@pytest.mark.timeout(600)
def test_flmexec_python_script_starts_app_with_numpy_dependency(check_flmexec_app_environment):
    script = textwrap.dedent(
        f"""
        import json
        import sys
        import traceback
        import uuid

        try:
            import flamepy.app as app

            app_name = f"test-flmexec-app-numpy-{{uuid.uuid4().hex[:8]}}"

            app.init(app_name, dependencies=["numpy"])
            try:
                @app.service()
                def service(limit):
                    import numpy as np

                    values = np.arange(1, limit + 1, dtype=np.int64)
                    return {{
                        "dtype": str(values.dtype),
                        "shape": list(values.shape),
                        "sum": int(values.sum()),
                    }}

                result = service.remote(5).get()
            finally:
                app.destroy()

            print("{NUMPY_RESULT_PREFIX}" + json.dumps(result, sort_keys=True))
        except BaseException:
            traceback.print_exc(file=sys.stdout)
            sys.stdout.flush()
            raise
        """
    )

    output = _invoke_flmexec_python(script)
    result_line = next((line for line in output.splitlines() if line.startswith(NUMPY_RESULT_PREFIX)), None)
    assert result_line is not None, f"missing result line in flmexec output:\n{output}"

    result = json.loads(result_line.removeprefix(NUMPY_RESULT_PREFIX))
    assert result == {"dtype": "int64", "shape": [5], "sum": 15}
