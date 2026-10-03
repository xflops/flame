import asyncio
import shutil
import subprocess
from types import SimpleNamespace

import grpc
import pytest

from flamepy.core import cache as cache_module
from flamepy.core.aio import cache as aio_cache
from flamepy.core.cache import FetchMode, FetchResult, Object, ObjectRef, Patch, _cache_lock, _object_cache
from flamepy.core.types import FlameClientTls
from flamepy.proto import cache_pb2, cache_pb2_grpc


class TestAioCache:
    def setup_method(self):
        with _cache_lock:
            _object_cache.clear()

    def teardown_method(self):
        with _cache_lock:
            _object_cache.clear()

    def test_endpoint_resolution_and_aio_channel(self, monkeypatch):
        calls = []
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=None))
        monkeypatch.setattr(aio_cache.grpc.aio, "insecure_channel", lambda target, options: calls.append((target, options)) or object())
        monkeypatch.setattr(aio_cache.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: channel)

        async def run():
            assert aio_cache._get_client("grpc://cache:9090") is aio_cache._get_client("grpc://cache:9090")
            aio_cache._clients.pop(asyncio.get_running_loop())

        asyncio.run(run())
        assert calls == [("cache:9090", cache_module.GRPC_OPTIONS)]

    def test_proxy_routes_owner_in_metadata(self, monkeypatch):
        calls = []
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint="grpcs-proxy://gateway.example:443")))
        monkeypatch.setattr(aio_cache.grpc, "ssl_channel_credentials", lambda **kwargs: object())
        monkeypatch.setattr(aio_cache.grpc.aio, "secure_channel", lambda target, credentials, options: calls.append((target, options)) or object())
        monkeypatch.setattr(aio_cache.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: SimpleNamespace(Get=lambda *args, **kwargs: (args, kwargs)))

        async def run():
            stub = aio_cache._get_client("grpc://object-cache:9090")
            assert stub.Get("request")[1]["metadata"] == [("x-flame-object-cache", "object-cache:9090")]
            aio_cache._clients.pop(asyncio.get_running_loop())

        asyncio.run(run())
        assert calls[0][0] == "gateway.example:443"
        assert not any(key == "grpc.default_authority" for key, _ in calls[0][1])

    def test_proxy_initial_authority_is_public(self, monkeypatch):
        calls = []
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint="grpcs-proxy://gateway.example:443")))
        monkeypatch.setattr(aio_cache.grpc, "ssl_channel_credentials", lambda **kwargs: object())
        monkeypatch.setattr(aio_cache.grpc.aio, "secure_channel", lambda target, credentials, options: calls.append((target, options)) or object())
        monkeypatch.setattr(aio_cache.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: SimpleNamespace(Get=lambda *args, **kwargs: (args, kwargs)))

        async def run():
            stub = aio_cache._get_client("grpcs-proxy://gateway.example:443")
            assert stub.Get("request")[1] == {}
            aio_cache._clients.pop(asyncio.get_running_loop())

        asyncio.run(run())
        assert not any(key == "grpc.default_authority" for key, _ in calls[0][1])

    def test_proxy_client_identity_and_owner_metadata(self, monkeypatch, tmp_path):
        ca = tmp_path / "ca.pem"
        cert = tmp_path / "client.pem"
        key = tmp_path / "client-key.pem"
        ca.write_bytes(b"ca")
        cert.write_bytes(b"certificate")
        key.write_bytes(b"private key")
        credentials = []
        channels = []
        monkeypatch.setattr(aio_cache.grpc, "ssl_channel_credentials", lambda **kwargs: credentials.append(kwargs) or object())
        monkeypatch.setattr(aio_cache.grpc.aio, "secure_channel", lambda target, creds, options: channels.append((target, options)) or object())
        methods = ("Put", "Get", "Patch", "GetMetadata", "Delete")
        monkeypatch.setattr(aio_cache.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: SimpleNamespace(**{method: lambda *args, **kwargs: kwargs for method in methods}))
        tls = FlameClientTls(str(ca), str(cert), str(key))

        async def run():
            for endpoint in ("grpc://cache-0:9090", "grpcs://cache-0:9090"):
                monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint="grpcs-proxy://gateway.example:443")))
                stub = aio_cache._get_client(endpoint, tls)
                for method in methods:
                    assert getattr(stub, method)("request")["metadata"] == [("x-flame-object-cache", "cache-0:9090")]
            aio_cache._clients.pop(asyncio.get_running_loop())

        asyncio.run(run())
        assert channels == [("gateway.example:443", cache_module.GRPC_OPTIONS)]
        assert credentials == [{"root_certificates": b"ca", "private_key": b"private key", "certificate_chain": b"certificate"}]

    def test_incomplete_client_identity_rejected(self):
        with pytest.raises(ValueError, match="cert_file and key_file"):
            FlameClientTls(cert_file="client.pem").validate_identity()
        with pytest.raises(ValueError, match="must not be empty"):
            FlameClientTls(cert_file="", key_file="").validate_identity()
        with pytest.raises(ValueError, match="cert_file and key_file"):
            aio_cache._create_client("grpcs-proxy://gateway.example:443", FlameClientTls(key_file="key.pem"), None)

    def test_plaintext_direct_cache_ignores_tls_identity(self, monkeypatch):
        monkeypatch.setattr(aio_cache.grpc.aio, "insecure_channel", lambda target, options: object())
        monkeypatch.setattr(aio_cache.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: channel)
        aio_cache._create_client("grpc://cache:9090", FlameClientTls(cert_file="client.pem"), None)

    def test_invalid_context_does_not_fall_back_to_direct_cache(self, monkeypatch):
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: (_ for _ in ()).throw(ValueError("bad TLS configuration")))
        with pytest.raises(ValueError, match="bad TLS configuration"):
            cache_module._resolve_cache_endpoint("grpc://cache:9090")

    def test_proxy_mtls_public_server_name_and_owner_route(self, tmp_path):
        if shutil.which("openssl") is None:
            pytest.skip("openssl is required for the TLS integration test")

        def openssl(*args):
            subprocess.run(["openssl", *map(str, args)], cwd=tmp_path, check=True, capture_output=True)

        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "1", "-subj", "/CN=Test CA")
        for name, extension in (("gateway", "subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n"), ("client", "extendedKeyUsage=clientAuth\n")):
            openssl("req", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{name}.key", "-out", f"{name}.csr", "-subj", f"/CN={name}")
            (tmp_path / f"{name}.ext").write_text(extension)
            openssl("x509", "-req", "-in", f"{name}.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", f"{name}.pem", "-days", "1", "-extfile", f"{name}.ext")

        class Service(cache_pb2_grpc.ObjectCacheServiceServicer):
            async def GetMetadata(self, request, context):  # noqa: N802 - gRPC method name
                routes.append(dict(context.invocation_metadata()).get("x-flame-object-cache"))
                return cache_pb2.CacheObjectMetadata(key=request.key)

        async def run():
            server = grpc.aio.server()
            cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(Service(), server)
            credentials = grpc.ssl_server_credentials(
                [((tmp_path / "gateway.key").read_bytes(), (tmp_path / "gateway.pem").read_bytes())],
                root_certificates=(tmp_path / "ca.pem").read_bytes(),
                require_client_auth=True,
            )
            port = server.add_secure_port("localhost:0", credentials)
            await server.start()
            try:
                endpoint = f"grpcs-proxy://localhost:{port}"
                tls = FlameClientTls(str(tmp_path / "ca.pem"), str(tmp_path / "client.pem"), str(tmp_path / "client.key"))
                channel, stub = aio_cache._create_client(endpoint, tls, "cache-0:9090")
                try:
                    await stub.GetMetadata(cache_pb2.CacheGetMetadataRequest(key="key"), timeout=5)
                finally:
                    await channel.close()
                assert routes == ["cache-0:9090"]

                for invalid_tls in (FlameClientTls(ca_file=str(tmp_path / "ca.pem")), FlameClientTls(cert_file=str(tmp_path / "client.pem"), key_file=str(tmp_path / "client.key"))):
                    channel, stub = aio_cache._create_client(endpoint, invalid_tls, "cache-0:9090")
                    try:
                        with pytest.raises(grpc.aio.AioRpcError):
                            await stub.GetMetadata(cache_pb2.CacheGetMetadataRequest(key="key"), timeout=2)
                    finally:
                        await channel.close()
                assert routes == ["cache-0:9090"]
            finally:
                await server.stop(0)

        routes = []
        asyncio.run(run())

    @pytest.mark.parametrize("endpoint", ["grpcs-proxy://gateway:443/path", "grpcs-proxy://gateway"])
    def test_invalid_proxy(self, endpoint):
        with pytest.raises(ValueError, match="proxy endpoint"):
            cache_module._resolve_cache_endpoint(endpoint)

    @pytest.mark.parametrize("endpoint", ["grpc://user@cache:9090", "grpc://cache:9090/path", "grpc://cache:9090?route=other", "grpc://cache", "grpc://cache:bad"])
    def test_invalid_owner_route(self, endpoint, monkeypatch):
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint="grpcs-proxy://gateway.example:443")))
        with pytest.raises(ValueError, match="Invalid object cache endpoint"):
            cache_module._resolve_cache_endpoint(endpoint)

    def test_full_cached_patch_and_version_zero(self, monkeypatch):
        versions = []

        async def fetch(ref, version):
            versions.append(version)
            if version == 0:
                return FetchResult(FetchMode.FULL, 2, [1], [Patch(2, [2])])
            return FetchResult(FetchMode.PATCHES, 3, None, [Patch(3, [3])])

        monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)
        ref = ObjectRef("grpc://cache:1", "default/app/session/key", 2)

        async def run():
            assert await aio_cache.get_object(ref, lambda base, patches: base + [item for patch in patches for item in patch]) == [1, 2]
            assert await aio_cache.get_object(ref, lambda base, patches: base + [item for patch in patches for item in patch]) == [1, 2, 3]
            assert await aio_cache.get_object(ObjectRef(ref.endpoint, ref.key, 0)) == [1]

        asyncio.run(run())
        assert versions == [0, 2, 0]

    def test_not_modified_and_patch_fallback(self, monkeypatch):
        versions = []
        key = ("grpc://cache:1", "default/app/session/key")
        cache_module._cache_put(key, Object(version=1, data="old"))

        async def fetch(ref, version):
            versions.append(version)
            if version == 1:
                return FetchResult(FetchMode.PATCHES, 2, None, [Patch(2, "delta")])
            return FetchResult(FetchMode.FULL, 2, "new")

        monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)
        ref = ObjectRef(*key, version=1)

        async def run():
            cache_module._cache_remove(key)
            assert await aio_cache.get_object(ref) == "new"
            monkeypatch.setattr(aio_cache, "_fetch_object_data", lambda ref, version: asyncio.sleep(0, result=None))
            assert await aio_cache.get_object(ref) == "new"

        asyncio.run(run())
        assert versions == [0]

    def test_delayed_older_fetch_does_not_regress_cache(self, monkeypatch):
        ref = ObjectRef("grpc://cache:1", "default/app/session/key", 0)
        first_started = asyncio.Event()
        release_first = asyncio.Event()
        calls = 0

        async def fetch(ref, version):
            nonlocal calls
            calls += 1
            if calls == 1:
                first_started.set()
                await release_first.wait()
                return FetchResult(FetchMode.FULL, 2, "old")
            return FetchResult(FetchMode.FULL, 3, "new")

        monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)

        async def run():
            older = asyncio.create_task(aio_cache.get_object(ref))
            await first_started.wait()
            assert await aio_cache.get_object(ref) == "new"
            release_first.set()
            assert await older == "new"

        asyncio.run(run())
        assert cache_module._cache_get((ref.endpoint, ref.key)).version == 3

    def test_not_modified_reuses_materialized_bound_method(self, monkeypatch):
        ref = ObjectRef("grpc://cache:1", "default/app/session/key", 1)
        cache_module._cache_put((ref.endpoint, ref.key), Object(version=1, data=[1], patches=[Patch(1, [2])]))
        calls = []

        class Collector:
            def merge(self, base, patches):
                calls.append(True)
                return base + patches[0]

        async def unchanged(ref, version):
            assert version == 1
            return None

        monkeypatch.setattr(aio_cache, "_fetch_object_data", unchanged)
        collector = Collector()

        async def run():
            assert await aio_cache.get_object(ref, collector.merge) == [1, 2]
            assert await aio_cache.get_object(ref, collector.merge) == [1, 2]

        asyncio.run(run())
        assert len(calls) == 1

    def test_patch_type_mismatch_does_not_write(self, monkeypatch):
        writes = []

        class FakeClient:
            async def GetMetadata(self, request):  # noqa: N802 - gRPC method
                return SimpleNamespace(data_type="numpy.zstd")

            async def Patch(self, requests, timeout=None):  # noqa: N802 - gRPC method
                writes.append(True)

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FakeClient())

        async def run():
            with pytest.raises(ValueError, match="does not match"):
                await aio_cache.patch_object(ObjectRef("grpc://cache:1", "default/app/session/key"), [1, 2])

        asyncio.run(run())
        assert not writes

    def test_get_rejects_invalid_stream_order(self):
        async def responses():
            yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_BASE, data=b"x"))

        async def run():
            with pytest.raises(ValueError, match="must start with a header"):
                await aio_cache._read_get_parts(responses())

        asyncio.run(run())

    def test_put_patch_get_and_delete_wire_contract(self, monkeypatch):
        stored = {}
        endpoint = "grpc://cache:9090"
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))

        class FakeClient:
            async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["header"] = items[0].header
                stored["payload"] = b"".join(item.data for item in items[1:])
                return SimpleNamespace(endpoint=endpoint, key="default/app/session/key", version=1)

            async def GetMetadata(self, request):  # noqa: N802 - gRPC method
                return SimpleNamespace(data_type=stored["header"].data_type)

            async def Patch(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["patch"] = b"".join(item.data for item in items[1:])
                return SimpleNamespace(endpoint=endpoint, key="default/app/session/key", version=2)

            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(mode=cache_pb2.CACHE_GET_MODE_FULL, version=2, data_type=stored["header"].data_type))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_BASE, version=1, data=stored["payload"]))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_PATCH, version=2, data=stored["patch"]))

                return responses()

            async def Delete(self, request):  # noqa: N802 - gRPC method
                stored["deleted"] = request.key

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FakeClient())

        async def run():
            ref = await aio_cache.put_object("default/app/session", [1])
            await aio_cache.patch_object(ref, [2])
            assert await aio_cache.get_object(ref, lambda base, patches: base + patches[0]) == [1, 2]
            await aio_cache.delete_objects("default/app/session")

        asyncio.run(run())
        assert stored["deleted"] == "default/app/session"
        assert stored["header"].data_type == "cloudpickle"

    def test_file_upload_download_and_zstd(self, monkeypatch, tmp_path):
        payload = b"abc" * 400000
        source = tmp_path / "source.tar.gz"
        source.write_bytes(payload)
        endpoint = "grpc://cache:9090"
        stored = {}
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))

        class FileClient:
            async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["type"] = items[0].header.data_type
                stored["payload"] = b"".join(item.data for item in items[1:])
                return SimpleNamespace(endpoint=endpoint, key="default/app/session/file.tar.gz", version=1)

            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(mode=cache_pb2.CACHE_GET_MODE_FULL, data_type=stored["type"], version=1))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_BASE, version=1, data=stored["payload"][:7]))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_BASE, version=1, data=stored["payload"][7:]))

                return responses()

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FileClient())

        async def run():
            ref = await aio_cache.upload_object("default/app/session/file.tar.gz", str(source))
            assert stored["type"] == "raw"
            assert stored["payload"] == payload
            output = tmp_path / "output.tar.gz"
            await aio_cache.download_object(ref, str(output))
            assert output.read_bytes() == payload
            stored["type"] = "raw.zstd"
            stored["payload"] = cache_module._compress_data(payload, "zstd")
            await aio_cache.download_object(ref, str(output))
            assert output.read_bytes() == payload

        asyncio.run(run())

    def test_download_rejects_patches_and_removes_partial_file(self, monkeypatch, tmp_path):
        class FakeClient:
            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(mode=cache_pb2.CACHE_GET_MODE_FULL, data_type="raw", version=2))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_PATCH, version=2, data=b"bad"))

                return responses()

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FakeClient())
        output = tmp_path / "bad"

        async def run():
            with pytest.raises(ValueError, match="raw base chunks only"):
                await aio_cache.download_object(ObjectRef("grpc://cache:1", "default/app/session/key"), str(output))

        asyncio.run(run())
        assert not output.exists()

    def test_upload_validates_path_before_rpc(self, tmp_path):
        async def run():
            with pytest.raises(FileNotFoundError):
                await aio_cache.upload_object("default/app/session/key", str(tmp_path / "missing"))
            source = tmp_path / "source"
            source.write_bytes(b"data")
            with pytest.raises(ValueError, match="Invalid object key"):
                await aio_cache.upload_object("bad", str(source))

        asyncio.run(run())

    def test_real_grpc_aio_transport(self, monkeypatch):
        class CacheService(cache_pb2_grpc.ObjectCacheServiceServicer):
            endpoint = ""
            objects = {}

            async def Put(self, request_iterator, context):  # noqa: N802 - gRPC method
                requests = [request async for request in request_iterator]
                key = "default/app/session/key"
                self.objects[key] = (requests[0].header.data_type, b"".join(request.data for request in requests[1:]), [])
                return cache_pb2.CacheObjectMetadata(endpoint=self.endpoint, key=key, version=1)

            async def Patch(self, request_iterator, context):  # noqa: N802 - gRPC method
                requests = [request async for request in request_iterator]
                key = requests[0].header.key
                data_type, base, patches = self.objects[key]
                patches.append(b"".join(request.data for request in requests[1:]))
                return cache_pb2.CacheObjectMetadata(endpoint=self.endpoint, key=key, version=1 + len(patches))

            async def GetMetadata(self, request, context):  # noqa: N802 - gRPC method
                data_type, _, patches = self.objects[request.key]
                return cache_pb2.CacheObjectMetadata(endpoint=self.endpoint, key=request.key, version=1 + len(patches), data_type=data_type)

            async def Get(self, request, context):  # noqa: N802 - gRPC method
                data_type, base, patches = self.objects[request.key]
                version = 1 + len(patches)
                if request.client_version == version:
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(mode=cache_pb2.CACHE_GET_MODE_NOT_MODIFIED, version=version))
                    return
                yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(mode=cache_pb2.CACHE_GET_MODE_FULL, version=version, data_type=data_type))
                yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_BASE, version=1, data=base))
                for index, patch in enumerate(patches, start=2):
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(kind=cache_pb2.CACHE_CHUNK_KIND_PATCH, version=index, data=patch))

        async def run():
            service = CacheService()
            server = grpc.aio.server()
            cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(service, server)
            port = server.add_insecure_port("127.0.0.1:0")
            service.endpoint = f"grpc://127.0.0.1:{port}"
            monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=service.endpoint))
            await server.start()
            try:
                ref = await aio_cache.put_object("default/app/session", [1])
                assert await aio_cache.get_object(ref) == [1]
                ref = await aio_cache.patch_object(ref, [2])
                assert await aio_cache.get_object(ref, lambda base, patches: base + patches[0]) == [1, 2]
                assert asyncio.get_running_loop() in aio_cache._clients
            finally:
                await aio_cache.close()
                assert asyncio.get_running_loop() not in aio_cache._clients
                await server.stop(0)

        asyncio.run(run())
