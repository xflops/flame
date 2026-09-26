"""Inline and cache-backed reference behavior."""

from unittest.mock import patch

import pytest

from flamepy.core import ObjectRef, Ref, ValueRef, get_object
from flamepy.core.aio import get_object as aio_get_object


def test_ref_subclasses_and_inline_sync_get():
    inline = ValueRef(None)
    cached = ObjectRef(endpoint="grpc://host:9090", key="app/session/value", version=1)
    assert isinstance(inline, Ref)
    assert isinstance(cached, Ref)

    with patch("flamepy.core.cache._call_aio_cache") as remote:
        assert get_object(inline) is None
        assert get_object(ValueRef(7), lambda value, patches: (value, patches)) == (7, [])
    remote.assert_not_called()


@pytest.mark.asyncio
async def test_inline_aio_get_avoids_remote_cache():
    with patch("flamepy.core.aio.cache._get_client") as client:
        assert await aio_get_object(ValueRef("value")) == "value"
        assert await aio_get_object(ValueRef(7), lambda value, patches: (value, patches)) == (7, [])
    client.assert_not_called()


def test_get_rejects_unrecognized_ref():
    with pytest.raises(TypeError, match="Expected ValueRef or ObjectRef"):
        get_object(Ref())
