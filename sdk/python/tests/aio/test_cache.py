import asyncio
import shutil
import subprocess
from types import SimpleNamespace

import cloudpickle
import grpc
import pytest

from flamepy.core import cache as cache_module
from flamepy.core.aio import cache as aio_cache
from flamepy.core.cache import FetchResult, ObjectRef, ValueRef, _cache_lock, _object_cache
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

    @staticmethod
    def _pickled(*values):
        return [cloudpickle.dumps(value) for value in values]

    def test_decode_parts_of_the_whole_object(self):
        header = SimpleNamespace(version=3, data_type="cloudpickle")
        parts = list(zip([1, 2, 3], self._pickled("first", "second", "third")))

        result = aio_cache._decode_parts(header, parts)

        assert (result.version, result.data) == (3, ["first", "second", "third"])

    def test_decode_parts_after_a_version(self):
        header = SimpleNamespace(version=4, data_type="cloudpickle")
        parts = list(zip([3, 4], self._pickled("third", "fourth")))

        result = aio_cache._decode_parts(header, parts)

        assert (result.version, result.data) == (4, ["third", "fourth"])

    def test_decode_parts_without_data_is_unchanged(self):
        assert aio_cache._decode_parts(SimpleNamespace(version=2, data_type=""), []) is None

    @pytest.mark.parametrize("versions", [[1, 3], [2, 1], [1, 2]])
    def test_decode_parts_rejects_inconsistent_versions(self, versions):
        header = SimpleNamespace(version=3, data_type="cloudpickle")
        parts = list(zip(versions, self._pickled(*range(len(versions)))))

        with pytest.raises(ValueError, match="consecutive"):
            aio_cache._decode_parts(header, parts)

    def test_inline_value_is_a_single_part(self):
        async def run():
            ref = ValueRef(7)
            assert await aio_cache.get_object(ref) == 7
            assert await aio_cache.get_object(ref, lambda parts: parts) == [7]

        asyncio.run(run())

    def test_get_creates_then_appends_and_version_zero_starts_over(self, monkeypatch):
        versions = []

        async def fetch(ref, version):
            versions.append(version)
            if version == 0:
                return FetchResult(2, [[1], [2]])
            return FetchResult(3, [[3]])

        monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)
        ref = ObjectRef("grpc://cache:1", "app/session/key", 2)

        async def run():
            assert await aio_cache.get_object(ref, lambda parts: [item for part in parts for item in part]) == [1, 2]
            assert await aio_cache.get_object(ref, lambda parts: [item for part in parts for item in part]) == [1, 2, 3]
            # Version 0 is a forced refresh: the cached object is dropped and made again.
            assert await aio_cache.get_object(ObjectRef(ref.endpoint, ref.key, 0)) == [2]

        asyncio.run(run())
        assert versions == [0, 2, 0]

    def test_cache_entry_dropped_while_fetching_gets_all_the_data(self, monkeypatch):
        key = ("grpc://cache:1", "app/session/key")
        versions = []

        async def fetch(ref, version):
            versions.append(version)
            if version == 2:
                # The cached object is dropped while the Get is in flight.
                cache_module._cache_remove(key)
                return FetchResult(3, [[3]])
            return FetchResult(3, [[1], [2], [3]])

        cache_module._cache_append_parts(key, FetchResult(2, [[1], [2]]))
        monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)
        ref = ObjectRef(*key, version=2)

        async def run():
            merged = await aio_cache.get_object(ref, lambda parts: [item for part in parts for item in part])
            assert merged == [1, 2, 3]

        asyncio.run(run())
        assert versions == [2, 0]
        assert cache_module._cache_get(key).version == 3

    def test_not_modified_and_patch_fallback(self, monkeypatch):
        versions = []
        key = ("grpc://cache:1", "app/session/key")
        cache_module._cache_append_parts(key, FetchResult(1, ["old"]))

        async def fetch(ref, version):
            versions.append(version)
            if version == 1:
                return FetchResult(2, ["second"])
            return FetchResult(2, ["first", "new"])

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
        ref = ObjectRef("grpc://cache:1", "app/session/key", 0)
        first_started = asyncio.Event()
        release_first = asyncio.Event()
        calls = 0

        async def fetch(ref, version):
            nonlocal calls
            calls += 1
            if calls == 1:
                first_started.set()
                await release_first.wait()
                return FetchResult(2, ["first", "old"])
            return FetchResult(3, ["first", "mid", "new"])

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
        ref = ObjectRef("grpc://cache:1", "app/session/key", 2)
        cache_module._cache_append_parts((ref.endpoint, ref.key), FetchResult(2, [[1], [2]]))
        calls = []

        class Collector:
            def merge(self, parts):
                calls.append(True)
                return parts[0] + parts[1]

        async def unchanged(ref, version):
            assert version == 2
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
                await aio_cache.patch_object(ObjectRef("grpc://cache:1", "app/session/key"), [1, 2])

        asyncio.run(run())
        assert not writes

    def test_get_rejects_invalid_stream_order(self):
        async def responses():
            yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=1, data=b"x"))

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
                return SimpleNamespace(endpoint=endpoint, key="app/session/key", version=1)

            async def GetMetadata(self, request):  # noqa: N802 - gRPC method
                return SimpleNamespace(data_type=stored["header"].data_type)

            async def Patch(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["patch"] = b"".join(item.data for item in items[1:])
                return SimpleNamespace(endpoint=endpoint, key="app/session/key", version=2)

            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(version=2, data_type=stored["header"].data_type))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=1, data=stored["payload"]))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=2, data=stored["patch"]))

                return responses()

            async def Delete(self, request):  # noqa: N802 - gRPC method
                stored["deleted"] = request.key

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FakeClient())

        async def run():
            ref = await aio_cache.put_object("app/session", [1])
            await aio_cache.patch_object(ref, [2])
            assert await aio_cache.get_object(ref, lambda parts: parts[0] + parts[1]) == [1, 2]
            await aio_cache.delete_objects("app/session")

        asyncio.run(run())
        assert stored["deleted"] == "app/session"
        assert stored["header"].data_type == "cloudpickle"

    def test_file_upload_download_and_zstd(self, monkeypatch, tmp_path):
        payload = b"abc" * 400000
        source = tmp_path / "source.tar.gz"
        source.write_bytes(payload)
        endpoint = "grpc://cache:9090"
        stored = {}
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))

        class FileClient:
            async def Delete(self, request):  # noqa: N802 - gRPC method
                return cache_pb2.CacheDeleteResponse()

            async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["type"] = items[0].header.data_type
                stored["payload"] = b"".join(item.data for item in items[1:])
                return SimpleNamespace(endpoint=endpoint, key="app/pkg/file.tar.gz", version=1)

            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(data_type=stored["type"], version=1))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=1, data=stored["payload"][:7]))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=1, data=stored["payload"][7:]))

                return responses()

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FileClient())

        async def run():
            ref = await aio_cache.upload_package("app", str(source), "file.tar.gz")
            assert ref.key == "app/pkg/file.tar.gz"
            assert stored["type"] == "raw"
            assert stored["payload"] == payload
            output = tmp_path / "output.tar.gz"
            await aio_cache.download_package("app", "file.tar.gz", str(output), endpoint="grpc://cache:1")
            assert output.read_bytes() == payload
            stored["type"] = "raw.zstd"
            stored["payload"] = cache_module._compress_data(payload, "zstd")
            await aio_cache.download_package("app", "file.tar.gz", str(output), endpoint="grpc://cache:1")
            assert output.read_bytes() == payload

        asyncio.run(run())

    def test_download_rejects_appended_data_and_removes_partial_file(self, monkeypatch, tmp_path):
        class FakeClient:
            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(data_type="raw", version=2))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=2, data=b"bad"))

                return responses()

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FakeClient())
        output = tmp_path / "bad"

        async def run():
            with pytest.raises(ValueError, match="raw data of the first version only"):
                await aio_cache.download_package("app", "key", str(output), endpoint="grpc://cache:1")

        asyncio.run(run())
        assert not output.exists()

    def test_package_functions_use_the_package_key(self, monkeypatch, tmp_path):
        calls = []

        async def delete_objects(key_prefix, endpoint=None):
            calls.append(("delete", key_prefix, endpoint))

        async def upload_file(key, file_path, endpoint):
            calls.append(("upload", key, endpoint))
            return ObjectRef("grpc://cache:1", key, 1)

        async def download_file(ref, dest_path):
            calls.append(("download", ref.key, ref.endpoint))

        monkeypatch.setattr(aio_cache, "delete_objects", delete_objects)
        monkeypatch.setattr(aio_cache, "_upload_file", upload_file)
        monkeypatch.setattr(aio_cache, "_download_file", download_file)
        source = tmp_path / "pkg.tar.gz"
        source.write_bytes(b"package")

        async def run():
            # An existing package is deleted first; the file name is the default.
            ref = await aio_cache.upload_package("app", str(source), endpoint="grpc://cache:1")
            assert ref.key == "app/pkg/pkg.tar.gz"
            await aio_cache.upload_package("app", str(source), "other.tar.gz", endpoint="grpc://cache:1")
            await aio_cache.download_package("app", "pkg.tar.gz", str(tmp_path / "out"), endpoint="grpc://cache:1")
            await aio_cache.delete_package("app", "pkg.tar.gz", endpoint="grpc://cache:1")
            with pytest.raises(FileNotFoundError):
                await aio_cache.upload_package("app", str(tmp_path / "missing"))

        asyncio.run(run())
        assert calls == [
            ("delete", "app/pkg/pkg.tar.gz", "grpc://cache:1"),
            ("upload", "app/pkg/pkg.tar.gz", "grpc://cache:1"),
            ("delete", "app/pkg/other.tar.gz", "grpc://cache:1"),
            ("upload", "app/pkg/other.tar.gz", "grpc://cache:1"),
            ("download", "app/pkg/pkg.tar.gz", "grpc://cache:1"),
            ("delete", "app/pkg/pkg.tar.gz", "grpc://cache:1"),
        ]

    def test_upload_validates_path_before_rpc(self, tmp_path):
        async def run():
            with pytest.raises(FileNotFoundError):
                await aio_cache.upload_package("app", str(tmp_path / "missing"))
            source = tmp_path / "source"
            source.write_bytes(b"data")
            with pytest.raises(ValueError, match="invalid characters"):
                await aio_cache.upload_package("bad/app", str(source))

        asyncio.run(run())

    def test_real_grpc_aio_transport(self, monkeypatch):
        class CacheService(cache_pb2_grpc.ObjectCacheServiceServicer):
            endpoint = ""
            objects = {}

            async def Put(self, request_iterator, context):  # noqa: N802 - gRPC method
                requests = [request async for request in request_iterator]
                key = "app/session/key"
                self.objects[key] = (requests[0].header.data_type, b"".join(request.data for request in requests[1:]), [])
                return cache_pb2.CacheObjectMetadata(endpoint=self.endpoint, key=key, version=1)

            async def Patch(self, request_iterator, context):  # noqa: N802 - gRPC method
                requests = [request async for request in request_iterator]
                key = requests[0].header.key
                data_type, first, appended = self.objects[key]
                appended.append(b"".join(request.data for request in requests[1:]))
                return cache_pb2.CacheObjectMetadata(endpoint=self.endpoint, key=key, version=1 + len(appended))

            async def GetMetadata(self, request, context):  # noqa: N802 - gRPC method
                data_type, _, appended = self.objects[request.key]
                return cache_pb2.CacheObjectMetadata(endpoint=self.endpoint, key=request.key, version=1 + len(appended), data_type=data_type)

            async def Get(self, request, context):  # noqa: N802 - gRPC method
                data_type, first, appended = self.objects[request.key]
                version = 1 + len(appended)
                if request.version == version:
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(version=version))
                    return
                if request.version > version:
                    await context.abort(grpc.StatusCode.INVALID_ARGUMENT, "version is ahead of the object")
                yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(version=version, data_type=data_type))
                # The data after the version asked for: all of it for 0.
                for index, data in enumerate([first, *appended], start=1):
                    if index > request.version:
                        yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(version=index, data=data))

        async def run():
            service = CacheService()
            server = grpc.aio.server()
            cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(service, server)
            port = server.add_insecure_port("127.0.0.1:0")
            service.endpoint = f"grpc://127.0.0.1:{port}"
            monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=service.endpoint))
            await server.start()
            try:
                ref = await aio_cache.put_object("app/session", [1])
                assert await aio_cache.get_object(ref) == [1]
                ref = await aio_cache.patch_object(ref, [2])
                assert await aio_cache.get_object(ref, lambda parts: parts[0] + parts[1]) == [1, 2]
                assert cache_module._cache_get((ref.endpoint, ref.key)).version == 2

                # The key holds another, shorter object now: the cached version
                # is ahead of it, so the server refuses it and the client starts over.
                data_type, payload = cache_module._encode_object_data(["created again"])
                service.objects[ref.key] = (data_type, payload, [])
                assert await aio_cache.get_object(ref) == ["created again"]
                assert cache_module._cache_get((ref.endpoint, ref.key)).version == 1
                assert asyncio.get_running_loop() in aio_cache._clients
            finally:
                await aio_cache.close()
                assert asyncio.get_running_loop() not in aio_cache._clients
                await server.stop(0)

        asyncio.run(run())
