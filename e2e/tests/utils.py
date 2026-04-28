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

import random
import string


def random_string(size=16) -> str:
    """Generate a random string of specified size.

    Args:
        size: Length of the random string (default: 16)

    Returns:
        A random string of the specified size
    """
    return "".join(random.choice(string.ascii_letters + string.digits) for _ in range(size))
