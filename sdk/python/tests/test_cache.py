import pyarrow as pa

from flamepy.core.cache import ObjectRef, _deserialize_object, _serialize_object


def test_objectref_encode_decode_roundtrip():
    ref = ObjectRef(endpoint="grpc://host:9090", key="sess-1/obj1", version=1)
    data = ref.encode()
    ref2 = ObjectRef.decode(data)
    assert ref2.endpoint == ref.endpoint
    assert ref2.key == ref.key
    assert ref2.version == ref.version


def test_serialize_deserialize_roundtrip():
    obj = {"a": 1, "b": [1, 2, 3]}
    batch = _serialize_object(obj)
    recovered = _deserialize_object(batch)
    assert recovered == obj


def test_get_object_with_fake_flight_client(monkeypatch):
    from flamepy.core.cache import get_object

    # Create base object and corresponding batch/table
    base = {"hello": "world"}
    batch = _serialize_object(base)
    table = pa.Table.from_batches([batch])

    class DummyReader:
        def read_all(self):
            return table

    class DummyFlightClient:
        def do_get(self, ticket):
            return DummyReader()

    monkeypatch.setattr("flamepy.core.cache._get_flight_client", lambda endpoint, tls_config=None: DummyFlightClient())

    ref = ObjectRef(endpoint="grpc://host:9090", key="sess-1/obj1", version=0)
    result = get_object(ref)
    assert result == base
