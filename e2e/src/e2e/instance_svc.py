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

import flamepy

from e2e.api import TestRequest, TestResponse, TestContext

instance = flamepy.FlameInstance()


@instance.entrypoint
def e2e_service_entrypoint(req: TestRequest) -> TestResponse:
    cxt = instance.context()
    data = cxt.common_data if cxt is not None else None

    if req.update_common_data:
        instance.update_context(TestContext(common_data=req.input))

    return TestResponse(output=req.input, common_data=data)


if __name__ == "__main__":
    instance.run()
