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
from flamepy.core import ObjectRef, get_object, put_object, update_object


def test_cache_put_and_get():
    """Test basic put and get operations."""
    application_id = "test-app"
    session_id = "test-session-001"
    test_data = {"message": "Hello, Flame!", "value": 42}

    # Put object
    ref = put_object(application_id, session_id, test_data)

    # Verify ObjectRef structure
    assert isinstance(ref, ObjectRef)
    assert ref.endpoint is not None
    assert ref.key is not None
    assert ref.key.startswith(f"{application_id}/{session_id}/")
    assert ref.version == 0

    # Get object
    retrieved_data = get_object(ref)

    # Verify data
    assert retrieved_data == test_data


def test_cache_update():
    """Test update operation."""
    application_id = "test-app"
    session_id = "test-session-002"
    original_data = {"count": 0}
    updated_data = {"count": 1}

    # Put original object
    ref = put_object(application_id, session_id, original_data)

    # Update object
    new_ref = update_object(ref, updated_data)

    # Verify ObjectRef structure (should have same key)
    assert new_ref.key == ref.key
    assert new_ref.endpoint == ref.endpoint

    # Get updated object
    retrieved_data = get_object(new_ref)

    # Verify updated data
    assert retrieved_data == updated_data


def test_cache_with_complex_objects():
    """Test caching complex Python objects."""
    application_id = "test-app"
    session_id = "test-session-003"

    class ComplexObject:
        def __init__(self, name, data):
            self.name = name
            self.data = data

        def __eq__(self, other):
            return self.name == other.name and self.data == other.data

    test_obj = ComplexObject("test", [1, 2, 3, {"nested": "value"}])

    # Put object
    ref = put_object(application_id, session_id, test_obj)

    # Get object
    retrieved_obj = get_object(ref)

    # Verify object
    assert isinstance(retrieved_obj, ComplexObject)
    assert retrieved_obj.name == test_obj.name
    assert retrieved_obj.data == test_obj.data


def test_objectref_encode_decode():
    """Test ObjectRef serialization and deserialization."""
    ref = ObjectRef(endpoint="grpc://127.0.0.1:9090", key="app123/session123/obj456", version=0)

    # Encode
    encoded = ref.encode()
    assert isinstance(encoded, bytes)

    # Decode
    decoded = ObjectRef.decode(encoded)
    assert decoded.endpoint == ref.endpoint
    assert decoded.key == ref.key
    assert decoded.version == ref.version
