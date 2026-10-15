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

System test setup: make the e2e package importable and register the service app.
"""

import sys
from pathlib import Path

TESTING = Path(__file__).resolve().parents[1]
for path in (TESTING / "e2e" / "src", TESTING / "e2e", Path(__file__).resolve().parent):
    if str(path) not in sys.path:
        sys.path.insert(0, str(path))

import flamepy  # noqa: E402
import pytest  # noqa: E402
from tests.utils import deploy_e2e_application  # noqa: E402

from system_common import FLM_SYSTEM_TEST_APP, _close_system_sessions, _unregister_system_application  # noqa: E402


@pytest.fixture(scope="module")
def setup_system_test_env():
    """Register the reusable service application; session-API tests opt in with usefixtures."""
    _close_system_sessions()
    _unregister_system_application()

    deploy_e2e_application(FLM_SYSTEM_TEST_APP)

    yield

    _close_system_sessions()
    _unregister_system_application()
