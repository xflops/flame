"""Portable client TLS configuration and real local gRPC authentication tests."""

import asyncio
import base64
import json
import traceback
from dataclasses import asdict
from pathlib import Path
from types import SimpleNamespace

import cloudpickle
import grpc
import pytest

from flamepy.core import cache as cache_module
from flamepy.core import client as sync_client
from flamepy.core import tls as tls_module
from flamepy.core._bridge import LoopThread
from flamepy.core.aio import cache as aio_cache
from flamepy.core.aio import client as aio_client
from flamepy.core.types import FlameClientCache, FlameClientTls, FlameContext, FlameError
from flamepy.proto import cache_pb2, cache_pb2_grpc
from flamepy.proto.frontend_pb2_grpc import add_FrontendServicer_to_server
from tests.test_fixture import FrontendFixture


def encoded(data):
    """Encode one PEM source using the portable YAML representation."""
    return base64.b64encode(data).decode("ascii")


def inline_config(material, identity=True):
    """Select CA-only TLS or a complete client identity from synthetic PEMs."""
    fields = {"ca_data": encoded(material["ca"])}
    if identity:
        fields.update(cert_data=encoded(material["client"]), key_data=encoded(material["client_key"]))
    return FlameClientTls(**fields)


@pytest.mark.parametrize("identity", [False, True])
def test_inline_credentials_are_decoded_in_memory(monkeypatch, tls_material, identity):
    def reject_file_read(*args):
        raise AssertionError("inline TLS must not open credential files")

    monkeypatch.setattr(Path, "read_bytes", reject_file_read)
    monkeypatch.setattr(tls_module.grpc, "ssl_channel_credentials", lambda **kw: kw)
    config = inline_config(tls_material, identity)
    result = tls_module.channel_credentials(config)
    assert result == {"root_certificates": tls_material["ca"], "private_key": tls_material["client_key"] if identity else None, "certificate_chain": tls_material["client"] if identity else None}
    assert config.key_data not in repr(config) if identity else True
    assert asdict(config)["ca_data"] == encoded(tls_material["ca"])


def test_file_credentials_and_mixed_sources_remain_supported(tmp_path, monkeypatch, tls_material):
    for name, value in (("ca", "ca"), ("cert", "client"), ("key", "client_key")):
        (tmp_path / name).write_bytes(tls_material[value])
    monkeypatch.setattr(tls_module.grpc, "ssl_channel_credentials", lambda **kw: kw)
    files = FlameClientTls(*(str(tmp_path / name) for name in ("ca", "cert", "key")))
    mixed = FlameClientTls(cert_file=str(tmp_path / "cert"), ca_data=encoded(tls_material["ca"]), key_data=encoded(tls_material["client_key"]))
    assert tls_module.channel_credentials(files) == tls_module.channel_credentials(mixed)


@pytest.mark.parametrize("credential", ["ca", "cert", "key"])
def test_conflicting_sources_are_rejected(credential, tls_material):
    config = inline_config(tls_material)
    setattr(config, f"{credential}_file", "unused.pem")
    with pytest.raises(ValueError, match=f"{credential}_file and {credential}_data"):
        tls_module.channel_credentials(config)


@pytest.mark.parametrize("fields", [{"cert_data": "YQ=="}, {"key_data": "YQ=="}, {"cert_file": "missing.pem"}, {"key_file": "missing.pem"}])
def test_incomplete_identity_is_rejected_before_file_io(fields):
    with pytest.raises(ValueError, match="certificate.*private key"):
        tls_module.channel_credentials(FlameClientTls(**fields))


@pytest.mark.parametrize("data", ["", "not-base64!", "YQ", "YR==", "YQ==\n", "é", "\ud800"])
def test_invalid_inline_encoding_is_rejected(data):
    with pytest.raises(ValueError):
        tls_module.channel_credentials(FlameClientTls(ca_data=data))


@pytest.mark.parametrize("field", ["ca_data", "cert_data", "key_data"])
def test_pem_sources_must_be_utf8(field, tls_material):
    config = inline_config(tls_material)
    setattr(config, field, encoded(b"\xff" + tls_material["client_key"]))
    with pytest.raises(ValueError, match=f"{field} must contain UTF-8 PEM"):
        tls_module.channel_credentials(config)


@pytest.mark.parametrize("field", ["ca_data", "cert_data", "key_data"])
def test_invalid_pem_never_exposes_private_material(field, tls_material):
    secret = b"PRIVATE-CREDENTIAL-SENTINEL"
    config = inline_config(tls_material)
    setattr(config, field, encoded(secret))
    with pytest.raises(ValueError) as caught:
        tls_module.channel_credentials(config)
    diagnostic = "".join(traceback.format_exception(caught.value))
    assert secret.decode() not in diagnostic
    assert encoded(secret) not in diagnostic
    assert encoded(tls_material["client_key"]) not in diagnostic
    assert encoded(secret) not in repr(config)


@pytest.mark.parametrize("field", ["ca_data", "cert_data", "key_data", "ca_file", "cert_file", "key_file"])
def test_invalid_credential_field_types_have_safe_errors(field, tls_material):
    config = inline_config(tls_material)
    setattr(config, field, {"secret": "PRIVATE-SENTINEL"})
    with pytest.raises(ValueError) as caught:
        tls_module.channel_credentials(config)
    assert "PRIVATE-SENTINEL" not in str(caught.value)


@pytest.mark.parametrize("field", ["ca_data", "cert_data"])
def test_certificate_parser_rejects_invalid_der_or_incomplete_pem(field, tls_material):
    for invalid in (b"-----BEGIN CERTIFICATE-----\nYQ==\n-----END CERTIFICATE-----\n", tls_material["client"] + b"-----BEGIN CERTIFICATE-----\n"):
        config = inline_config(tls_material)
        setattr(config, field, encoded(invalid))
        with pytest.raises(ValueError, match="PEM"):
            tls_module.channel_credentials(config)


def test_private_key_parser_rejects_invalid_der_encryption_and_extra_keys(tls_material):
    from cryptography.hazmat.primitives import serialization

    key = serialization.load_pem_private_key(tls_material["client_key"], password=None)
    encrypted = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.BestAvailableEncryption(b"test-password"))
    for invalid in (b"-----BEGIN PRIVATE KEY-----\nYQ==\n-----END PRIVATE KEY-----\n", encrypted, tls_material["client_key"] * 2):
        config = inline_config(tls_material)
        config.key_data = encoded(invalid)
        with pytest.raises(ValueError, match="PEM"):
            tls_module.channel_credentials(config)


def test_mismatched_identity_is_rejected(tls_material):
    config = inline_config(tls_material)
    config.key_data = encoded(tls_material["server_key"])
    with pytest.raises(ValueError, match="do not match"):
        tls_module.channel_credentials(config)


def test_system_roots_without_explicit_credentials(monkeypatch):
    monkeypatch.setattr(tls_module.grpc, "ssl_channel_credentials", lambda **kw: kw)
    assert tls_module.channel_credentials(None) == {}
    assert tls_module.channel_credentials(FlameClientTls()) == {"root_certificates": None, "private_key": None, "certificate_chain": None}


def test_context_loads_inline_tls_and_preserves_it_over_file_environment(tmp_path, monkeypatch, tls_material):
    monkeypatch.setenv("HOME", str(tmp_path))
    directory = tmp_path / ".flame"
    directory.mkdir()
    fields = asdict(inline_config(tls_material))
    fields = {key: value for key, value in fields.items() if value is not None}
    (directory / "flame.yaml").write_text(json.dumps({"current-context": "inline", "contexts": [{"name": "inline", "cluster": {"endpoint": "https://localhost:8080", "tls": fields}, "cache": {"endpoint": "grpcs-proxy://localhost:9090", "tls": fields}}]}))
    for name in ("CA", "CERT", "KEY"):
        monkeypatch.setenv(f"FLAME_{name}_FILE", "unused-env-file")
    config = FlameContext()
    assert asdict(config.tls) == asdict(inline_config(tls_material))
    assert asdict(config.cache.tls) == asdict(config.tls)


def test_malformed_yaml_does_not_echo_inline_private_key(tmp_path, monkeypatch, tls_material):
    monkeypatch.setenv("HOME", str(tmp_path))
    directory = tmp_path / ".flame"
    directory.mkdir()
    secret = encoded(tls_material["client_key"])
    (directory / "flame.yaml").write_text(f'current-context: inline\nkey_data: "{secret}\\q"\n')
    with pytest.raises(FlameError) as caught:
        FlameContext()
    diagnostic = "".join(traceback.format_exception(caught.value))
    assert secret not in diagnostic
    assert "line 2" in diagnostic


def test_cache_pool_separates_inline_credentials_without_exposing_them(monkeypatch, tls_material):
    made = []
    monkeypatch.setattr(aio_cache, "_create_client", lambda *args: made.append(args) or (object(), object()))
    monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=None))
    first = inline_config(tls_material)
    second = inline_config(tls_material)
    second.cert_data = encoded(tls_material["server"])
    second.key_data = encoded(tls_material["server_key"])

    async def run():
        a = aio_cache._get_client("grpcs://localhost:9090", first)
        assert aio_cache._get_client("grpcs://localhost:9090", first) is a
        assert aio_cache._get_client("grpcs://localhost:9090", second) is not a
        pool = aio_cache._clients.pop(asyncio.get_running_loop())
        assert first.key_data not in repr(pool)
        assert second.key_data not in repr(pool)

    asyncio.run(run())
    assert len(made) == 2


@pytest.fixture(params=[False, True], ids=["server-tls", "mutual-tls"])
def tls_server(request, tls_material):
    """Serve frontend/cache RPCs with real TLS on an independent local loop."""
    routes = []

    class Cache(cache_pb2_grpc.ObjectCacheServiceServicer):
        async def Get(self, req, context):  # noqa: N802 - generated gRPC name
            routes.append(dict(context.invocation_metadata()).get("x-flame-object-cache"))
            yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=req.key, uuid="tls-object", version=1, data_type="cloudpickle")))
            if req.mode != cache_pb2.METADATA:
                yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=0, data=cloudpickle.dumps(b"tls-payload")))

    loop = LoopThread("tls-test-server")

    async def start():
        server = grpc.aio.server()
        add_FrontendServicer_to_server(FrontendFixture(), server)
        cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(Cache(), server)
        creds = grpc.ssl_server_credentials([(tls_material["server_key"], tls_material["server"])], root_certificates=tls_material["ca"] if request.param else None, require_client_auth=request.param)
        port = server.add_secure_port("localhost:0", creds)
        await server.start()
        return server, port

    server, port = loop.call(start())
    try:
        yield port, request.param, routes
    finally:
        loop.call(server.stop(0))
        loop.close()


def test_sync_cluster_uses_inline_tls(tls_server, tls_material):
    port, mutual, _ = tls_server
    connection = sync_client.connect(f"https://localhost:{port}", inline_config(tls_material, mutual))
    try:
        assert len(connection.list_applications()) == 1
    finally:
        connection.close()


def test_annotated_combined_pem_files_remain_compatible(tls_server, tls_material, tmp_path):
    port, _, _ = tls_server
    authority = tmp_path / "authority.pem"
    identity = tmp_path / "identity.pem"
    authority.write_bytes(b"Bag Attributes\n    friendlyName: test authority\n" + tls_material["ca"])
    identity.write_bytes(b"Bag Attributes\n    friendlyName: test identity\n" + tls_material["client"] + tls_material["client_key"])
    config = FlameClientTls(str(authority), str(identity), str(identity))
    connection = sync_client.connect(f"https://localhost:{port}", config)
    try:
        assert len(connection.list_applications()) == 1
    finally:
        connection.close()


def test_async_cluster_uses_inline_tls(tls_server, tls_material):
    port, mutual, _ = tls_server

    async def run():
        connection = await aio_client.connect(f"https://localhost:{port}", inline_config(tls_material, mutual))
        try:
            assert len(await connection.list_applications()) == 1
        finally:
            await connection.close()

    asyncio.run(run())


def test_sync_cache_uses_inline_tls_and_proxy_owner_metadata(tls_server, tls_material, monkeypatch):
    port, mutual, routes = tls_server
    config = FlameClientCache(endpoint=f"grpcs-proxy://localhost:{port}", tls=inline_config(tls_material, mutual))
    monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=config))
    ref = cache_module.ObjectRef("grpc://owner.example:9090", "tls/session/sync", 0)
    try:
        assert cache_module.get_object(ref) == b"tls-payload"
        assert routes == ["owner.example:9090"]
    finally:
        cache_module._cache_remove((ref.endpoint, ref.key))
        cache_module._close_aio_bridge()


def test_async_cache_uses_inline_tls_and_proxy_owner_metadata(tls_server, tls_material, monkeypatch):
    port, mutual, routes = tls_server
    config = FlameClientCache(endpoint=f"grpcs-proxy://localhost:{port}", tls=inline_config(tls_material, mutual))
    monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=config))
    ref = cache_module.ObjectRef("grpc://owner.example:9090", "tls/session/async", 0)

    async def run():
        try:
            assert await aio_cache.get_object(ref) == b"tls-payload"
            assert routes == ["owner.example:9090"]
        finally:
            await aio_cache.close()
            cache_module._cache_remove((ref.endpoint, ref.key))

    asyncio.run(run())


def test_inline_tls_still_rejects_wrong_server_name(tls_server, tls_material):
    port, mutual, _ = tls_server

    async def run():
        channel, stub = aio_cache._create_client(f"grpcs://127.0.0.1:{port}", inline_config(tls_material, mutual), None)
        try:
            with pytest.raises((grpc.aio.AioRpcError, asyncio.TimeoutError)):
                await asyncio.wait_for(aio_cache._get_metadata(stub, "tls/session/wrong-name"), timeout=2)
        finally:
            await channel.close()

    asyncio.run(run())


def test_inline_identity_does_not_disable_server_ca_verification(tls_server, tls_material):
    port, _, _ = tls_server
    config = inline_config(tls_material)
    config.ca_data = None

    async def run():
        channel, stub = aio_cache._create_client(f"grpcs://localhost:{port}", config, None)
        try:
            with pytest.raises(grpc.aio.AioRpcError) as caught:
                await asyncio.wait_for(aio_cache._get_metadata(stub, "tls/session/untrusted"), timeout=5)
            assert caught.value.code() == grpc.StatusCode.UNAVAILABLE
        finally:
            await channel.close()

    asyncio.run(run())


def test_ca_only_credentials_obey_server_client_auth_policy(tls_server, tls_material):
    port, mutual, _ = tls_server

    async def run():
        channel, stub = aio_cache._create_client(f"grpcs://localhost:{port}", inline_config(tls_material, identity=False), None)
        try:
            request = aio_cache._get_metadata(stub, "tls/session/client-auth")
            if mutual:
                with pytest.raises(grpc.aio.AioRpcError) as caught:
                    await asyncio.wait_for(request, timeout=5)
                assert caught.value.code() == grpc.StatusCode.UNAVAILABLE
            else:
                assert (await asyncio.wait_for(request, timeout=5)).version == 1
        finally:
            await channel.close()

    asyncio.run(run())
