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

    def test_proxy_client_identity_and_owner_metadata(self, monkeypatch, tmp_path, tls_material):
        ca = tmp_path / "ca.pem"
        cert = tmp_path / "client.pem"
        key = tmp_path / "client-key.pem"
        ca.write_bytes(tls_material["ca"])
        cert.write_bytes(tls_material["client"])
        key.write_bytes(tls_material["client_key"])
        credentials = []
        channels = []
        monkeypatch.setattr(aio_cache.grpc, "ssl_channel_credentials", lambda **kwargs: credentials.append(kwargs) or object())
        monkeypatch.setattr(aio_cache.grpc.aio, "secure_channel", lambda target, creds, options: channels.append((target, options)) or object())
        methods = ("Put", "Get", "Patch", "Delete")
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
        assert credentials == [{"root_certificates": tls_material["ca"], "private_key": tls_material["client_key"], "certificate_chain": tls_material["client"]}]

    def test_incomplete_client_identity_rejected(self):
        with pytest.raises(ValueError, match="certificate.*private key"):
            FlameClientTls(cert_file="client.pem").validate_identity()
        with pytest.raises(ValueError, match="must not be empty"):
            FlameClientTls(cert_file="", key_file="").validate_identity()
        with pytest.raises(ValueError, match="certificate.*private key"):
            aio_cache._create_client("grpcs-proxy://gateway.example:443", FlameClientTls(key_file="key.pem"), None)

    @pytest.mark.parametrize("scheme", ["grpc", "grpc-proxy"])
    def test_plaintext_cache_ignores_tls_identity(self, monkeypatch, scheme):
        monkeypatch.setattr(aio_cache.grpc.aio, "insecure_channel", lambda target, options: object())
        monkeypatch.setattr(aio_cache.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: channel)
        aio_cache._create_client(f"{scheme}://cache:9090", FlameClientTls(cert_file="client.pem"), None)

    def test_plaintext_proxy_initial_and_owner_routes(self, monkeypatch):
        routes = []

        class Service(cache_pb2_grpc.ObjectCacheServiceServicer):
            async def Get(self, request, context):  # noqa: N802 - gRPC method name
                routes.append(dict(context.invocation_metadata()).get("x-flame-object-cache"))
                yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", version=1, data_type="raw")))

        async def run():
            server = grpc.aio.server()
            cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(Service(), server)
            port = server.add_insecure_port("127.0.0.1:0")
            await server.start()
            endpoint = f"grpc-proxy://127.0.0.1:{port}"
            monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint=endpoint)))
            try:
                for origin in (endpoint, "grpc://cache-0:9090", "grpcs://cache-1:9090"):
                    response = await aio_cache._get_metadata(aio_cache._get_client(origin), "key")
                    assert response.key == "key"
                assert routes == [None, "cache-0:9090", "cache-1:9090"]
            finally:
                clients = aio_cache._clients.pop(asyncio.get_running_loop(), {})
                for channel, _ in clients.values():
                    await channel.close()
                await server.stop(0)

        asyncio.run(run())

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
            async def Get(self, request, context):  # noqa: N802 - gRPC method name
                routes.append(dict(context.invocation_metadata()).get("x-flame-object-cache"))
                yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", version=1, data_type="raw")))

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
                    await aio_cache._get_metadata(stub, "key")
                finally:
                    await channel.close()
                assert routes == ["cache-0:9090"]

                for invalid_tls in (FlameClientTls(ca_file=str(tmp_path / "ca.pem")), FlameClientTls(cert_file=str(tmp_path / "client.pem"), key_file=str(tmp_path / "client.key"))):
                    channel, stub = aio_cache._create_client(endpoint, invalid_tls, "cache-0:9090")
                    try:
                        with pytest.raises(grpc.aio.AioRpcError):
                            await aio_cache._get_metadata(stub, "key")
                    finally:
                        await channel.close()
                assert routes == ["cache-0:9090"]
            finally:
                await server.stop(0)

        routes = []
        asyncio.run(run())

    @pytest.mark.parametrize("endpoint", [f"{scheme}://{address}" for scheme in ("grpc-proxy", "grpcs-proxy") for address in ("gateway:443/path", "gateway", "user@gateway:443", "gateway:443?route=other", "gateway:bad")])
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
        header = SimpleNamespace(version=3, data_type="cloudpickle", uuid="object-uuid")
        parts = list(zip([0, 1, 2], self._pickled("first", "second", "third")))

        result = aio_cache._decode_parts(header, parts)

        assert (result.version, result.data) == (3, ["first", "second", "third"])

    def test_decode_parts_after_a_version(self):
        header = SimpleNamespace(version=4, data_type="cloudpickle", uuid="object-uuid")
        parts = list(zip([2, 3], self._pickled("third", "fourth")))

        result = aio_cache._decode_parts(header, parts, 2, "object-uuid", cache_pb2.PATCHES)

        assert (result.version, result.data) == (4, ["third", "fourth"])

    def test_decode_parts_without_data_is_unchanged(self):
        assert aio_cache._decode_parts(SimpleNamespace(version=2, data_type="cloudpickle", uuid="object-uuid"), [], 2, "object-uuid", cache_pb2.PATCHES) is None

    @pytest.mark.parametrize("versions", [[1, 3], [2, 1], [1, 2]])
    def test_decode_parts_rejects_inconsistent_versions(self, versions):
        header = SimpleNamespace(version=3, data_type="cloudpickle", uuid="object-uuid")
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

        async def fetch(ref, version, uuid=""):
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

        async def fetch(ref, version, uuid=""):
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

        async def fetch(ref, version, uuid=""):
            versions.append(version)
            if version == 1:
                return FetchResult(2, ["second"])
            return FetchResult(2, ["first", "new"])

        monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)
        ref = ObjectRef(*key, version=1)

        async def run():
            cache_module._cache_remove(key)
            assert await aio_cache.get_object(ref) == "new"
            monkeypatch.setattr(aio_cache, "_fetch_object_data", lambda ref, version, uuid="": asyncio.sleep(0, result=None))
            assert await aio_cache.get_object(ref) == "new"

        asyncio.run(run())
        assert versions == [0]

    def test_delayed_older_fetch_does_not_regress_cache(self, monkeypatch):
        ref = ObjectRef("grpc://cache:1", "app/session/key", 0)
        first_started = asyncio.Event()
        release_first = asyncio.Event()
        calls = 0

        async def fetch(ref, version, uuid=""):
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

        async def unchanged(ref, version, uuid=""):
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
            async def Get(self, request):  # noqa: N802 - gRPC method
                yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", version=1, data_type="numpy.zstd")))

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
            yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=0, data=b"x"))

        async def run():
            with pytest.raises(ValueError, match="must start with a header"):
                await aio_cache._read_get_parts(responses())

        asyncio.run(run())

    def test_put_patch_get_wire_contract(self, monkeypatch):
        stored = {}
        endpoint = "grpc://cache:9090"
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))

        class FakeClient:
            async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["header"] = items[0].header
                stored["payload"] = b"".join(item.data for item in items[1:])
                assert items[0].header.size == len(stored["payload"])
                return SimpleNamespace(endpoint=endpoint, key="app/session/key", version=1)

            async def Patch(self, requests, timeout=None):  # noqa: N802 - gRPC method
                items = [item async for item in requests]
                stored["patch"] = b"".join(item.data for item in items[1:])
                assert items[0].header.size == len(stored["patch"])
                return SimpleNamespace(endpoint=endpoint, key="app/session/key", version=2)

            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", version=1 + int("patch" in stored), data_type=stored["header"].data_type)))
                    if request.mode == cache_pb2.METADATA:
                        return
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=0, data=stored["payload"]))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=1, data=stored["patch"]))

                return responses()

        monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: FakeClient())

        async def run():
            ref = await aio_cache.put_object("app/session", [1])
            await aio_cache.patch_object(ref, [2])
            assert await aio_cache.get_object(ref, lambda parts: parts[0] + parts[1]) == [1, 2]

        asyncio.run(run())
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
                assert items[0].header.size == len(stored["payload"])
                return SimpleNamespace(endpoint=endpoint, key="app/pkg/file.tar.gz", version=1)

            def Get(self, request):  # noqa: N802 - gRPC method
                async def responses():
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", data_type=stored["type"], version=1)))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=0, data=stored["payload"][:7]))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=0, data=stored["payload"][7:]))

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
                    yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", data_type="raw", version=2)))
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=1, data=b"bad"))

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

        async def upload_file(key, file_path, endpoint, source=None):
            calls.append(("upload", key, endpoint))
            return ObjectRef("grpc://cache:1", key, 1)

        async def download_file(ref, dest_path):
            calls.append(("download", ref.key, ref.endpoint))

        monkeypatch.setattr(aio_cache, "_upload_file", upload_file)
        monkeypatch.setattr(aio_cache, "_download_file", download_file)
        source = tmp_path / "pkg.tar.gz"
        source.write_bytes(b"package")

        async def run():
            # The file name selects the package key by default.
            ref = await aio_cache.upload_package("app", str(source), endpoint="grpc://cache:1")
            assert ref.key == "app/pkg/pkg.tar.gz"
            await aio_cache.upload_package("app", str(source), "other.tar.gz", endpoint="grpc://cache:1")
            await aio_cache.download_package("app", "pkg.tar.gz", str(tmp_path / "out"), endpoint="grpc://cache:1")
            with pytest.raises(FileNotFoundError):
                await aio_cache.upload_package("app", str(tmp_path / "missing"))

        asyncio.run(run())
        assert calls == [
            ("upload", "app/pkg/pkg.tar.gz", "grpc://cache:1"),
            ("upload", "app/pkg/other.tar.gz", "grpc://cache:1"),
            ("download", "app/pkg/pkg.tar.gz", "grpc://cache:1"),
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

            uuid = "initial-object"

            async def Get(self, request, context):  # noqa: N802 - gRPC method
                data_type, first, appended = self.objects[request.key]
                parts = [first, *appended]
                version = len(parts)
                if request.mode != cache_pb2.METADATA:
                    if request.uuid and request.uuid != self.uuid:
                        await context.abort(grpc.StatusCode.FAILED_PRECONDITION, "object UUID differs")
                    if request.version > version:
                        await context.abort(grpc.StatusCode.INVALID_ARGUMENT, "version is ahead of the object")
                end = request.version if request.mode == cache_pb2.OBJECT and request.version else version
                yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid=self.uuid, version=end, data_type=data_type)))
                if request.mode == cache_pb2.METADATA:
                    return
                start = request.version if request.mode == cache_pb2.PATCHES else 0
                for index in range(start, end):
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=index, data=parts[index]))

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
                service.uuid = "recreated-object"
                service.objects[ref.key] = (data_type, payload, [])
                assert await aio_cache.get_object(ref) == ["created again"]
                assert cache_module._cache_get((ref.endpoint, ref.key)).version == 1
                assert asyncio.get_running_loop() in aio_cache._clients
            finally:
                await aio_cache.close()
                assert asyncio.get_running_loop() not in aio_cache._clients
                await server.stop(0)

        asyncio.run(run())


@pytest.mark.parametrize("count", [1, 2, 3])
def test_recreated_key_replaces_parts_even_when_count_is_equal_or_greater(monkeypatch, count):
    requests = []

    class Service(cache_pb2_grpc.ObjectCacheServiceServicer):
        uuid = "old-object"
        values = ["old-0", "old-1"]

        async def Get(self, request, context):  # noqa: N802
            requests.append((request.mode, request.version, request.uuid))
            if request.uuid and request.uuid != self.uuid:
                await context.abort(grpc.StatusCode.FAILED_PRECONDITION, "object was recreated")
            metadata = cache_pb2.CacheObjectMetadata(key=request.key, uuid=self.uuid, version=len(self.values), data_type="cloudpickle")
            yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=metadata))
            start = request.version if request.mode == cache_pb2.PATCHES else 0
            for index, value in enumerate(self.values[start:], start=start):
                payload = cloudpickle.dumps(value)
                for data in (payload[:2], payload[2:]):
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=index, data=data))

    async def run():
        service = Service()
        server = grpc.aio.server()
        cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(service, server)
        port = server.add_insecure_port("127.0.0.1:0")
        endpoint = f"grpc://127.0.0.1:{port}"
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))
        ref = ObjectRef(endpoint, "audit/session/recreated", 2)
        await server.start()
        try:
            assert await aio_cache.get_object(ref, list) == ["old-0", "old-1"]
            service.uuid = "new-object"
            service.values = [f"new-{part}" for part in range(count)]
            assert await aio_cache.get_object(ref, list) == service.values
            assert cache_module._cache_get((endpoint, ref.key)).uuid == "new-object"
            assert requests == [(cache_pb2.OBJECT, 0, ""), (cache_pb2.PATCHES, 2, "old-object"), (cache_pb2.OBJECT, 0, "")]
        finally:
            cache_module._cache_remove((endpoint, ref.key))
            await aio_cache.close()
            await server.stop(0)

    asyncio.run(run())


def test_delayed_old_uuid_response_cannot_replace_new_cached_object(monkeypatch):
    ref = ObjectRef("grpc://cache:1", "audit/session/delayed-uuid", 0)
    started = asyncio.Event()
    release = asyncio.Event()
    calls = 0

    async def fetch(ref, version, uuid=""):
        nonlocal calls
        calls += 1
        if calls == 1:
            started.set()
            await release.wait()
            return FetchResult(2, ["old-0", "old-1"], uuid="old")
        if version == 0:
            return FetchResult(2, ["new-0", "new-1"], uuid="new")
        assert (version, uuid) == (2, "new")
        return None

    monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)

    async def run():
        first = asyncio.create_task(aio_cache.get_object(ref, list))
        await started.wait()
        assert await aio_cache.get_object(ref, list) == ["new-0", "new-1"]
        release.set()
        assert await first == ["new-0", "new-1"]

    try:
        asyncio.run(run())
        assert cache_module._cache_get((ref.endpoint, ref.key)).uuid == "new"
    finally:
        cache_module._cache_remove((ref.endpoint, ref.key))


def test_invalidation_of_an_absent_entry_rejects_its_inflight_fetch(monkeypatch):
    ref = ObjectRef("grpc://cache:1", "audit/session/absent-invalidation", 1)
    started = asyncio.Event()
    release = asyncio.Event()
    calls = 0

    async def fetch(ref, version, uuid=""):
        nonlocal calls
        calls += 1
        assert version == 0
        if calls == 1:
            started.set()
            await release.wait()
            return FetchResult(1, ["old"], uuid="old")
        return FetchResult(1, ["new"], uuid="new")

    monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)

    async def run():
        read = asyncio.create_task(aio_cache.get_object(ref))
        await started.wait()
        cache_module._cache_remove((ref.endpoint, ref.key))
        release.set()
        assert await read == "new"

    try:
        asyncio.run(run())
        assert calls == 2
    finally:
        cache_module._cache_remove((ref.endpoint, ref.key))


def test_metadata_get_does_not_advance_cached_payload_count(monkeypatch):
    key = ("grpc://cache:1", "audit/session/metadata")
    cache_module._cache_append_parts(key, FetchResult(1, ["part-0"], uuid="object"))

    class Client:
        async def Get(self, request):  # noqa: N802
            assert request.mode == cache_pb2.METADATA
            metadata = cache_pb2.CacheObjectMetadata(key=request.key, uuid="object", version=3, data_type="raw", size=100)
            yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=metadata))

    async def run():
        metadata = await aio_cache._get_metadata(Client(), key[1])
        assert metadata.version == 3
        assert cache_module._cache_get(key).version == 1
        assert cache_module._cache_get(key).data == ["part-0"]

    try:
        asyncio.run(run())
    finally:
        cache_module._cache_remove(key)


def test_object_prefix_and_patches_use_zero_based_part_ranges():
    metadata = SimpleNamespace(version=2, uuid="object", data_type="cloudpickle")
    payload = [(index, cloudpickle.dumps(f"part-{index}")) for index in range(2)]
    prefix = aio_cache._decode_parts(metadata, payload, 2, "object", cache_pb2.OBJECT)
    assert prefix.data == ["part-0", "part-1"]
    assert prefix.version == 2
    suffix = aio_cache._decode_parts(metadata, payload[1:], 1, "object", cache_pb2.PATCHES)
    assert suffix.data == ["part-1"]
    for parts in (payload[1:], [(0, payload[0][1])]):
        with pytest.raises(ValueError, match="consecutive"):
            aio_cache._decode_parts(metadata, parts)
    with pytest.raises(ValueError, match="ahead"):
        aio_cache._decode_parts(metadata, [], 3, "object", cache_pb2.PATCHES)


@pytest.mark.parametrize("patch", [False, True])
def test_file_read_failure_does_not_commit_an_upload(patch):
    committed = []
    finished = asyncio.Event()

    class Service(cache_pb2_grpc.ObjectCacheServiceServicer):
        async def Put(self, requests, context):  # noqa: N802
            try:
                messages = [request async for request in requests]
                data = b"".join(request.data for request in messages[1:])
                # Cancellation may appear as EOF. Match storage's declared-size
                # check rather than treating any stream ending as a commit.
                if not messages or len(data) != messages[0].header.size:
                    await context.abort(grpc.StatusCode.INVALID_ARGUMENT, "incomplete upload")
                committed.append(data)
                return cache_pb2.CacheObjectMetadata(key="audit/pkg/file", endpoint="grpc://cache:1", version=1)
            finally:
                finished.set()

        async def Patch(self, requests, context):  # noqa: N802
            return await self.Put(requests, context)

    async def run():
        server = grpc.aio.server()
        cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(Service(), server)
        port = server.add_insecure_port("127.0.0.1:0")
        channel = grpc.aio.insecure_channel(f"127.0.0.1:{port}")
        client = cache_pb2_grpc.ObjectCacheServiceStub(channel)
        await server.start()
        try:

            async def chunks():
                yield b"uncommitted-prefix"
                raise OSError("source read failed")

            with pytest.raises(OSError, match="source read failed"):
                await aio_cache._write_remote(client, "audit/pkg/file", "raw", chunks(), size=64, patch=patch)
            await asyncio.wait_for(finished.wait(), timeout=5)
            assert committed == []
        finally:
            await channel.close()
            await server.stop(0)

    asyncio.run(run())


def test_active_fetch_tokens_are_not_evicted_with_cache_capacity(monkeypatch):
    monkeypatch.setattr(cache_module, "_CACHE_MAX_SIZE", 1)
    refs = [ObjectRef("grpc://cache:1", f"audit/session/concurrent-{part}", 1) for part in range(2)]
    started = [asyncio.Event(), asyncio.Event()]
    release = [asyncio.Event(), asyncio.Event()]
    calls = [0, 0]

    async def fetch(ref, version, uuid=""):
        part = int(ref.key[-1])
        calls[part] += 1
        assert calls[part] == 1, "an active request token was discarded by unrelated cache pressure"
        started[part].set()
        await release[part].wait()
        return FetchResult(1, [part], uuid=f"object-{part}")

    monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)

    async def run():
        first = asyncio.create_task(aio_cache.get_object(refs[0]))
        await started[0].wait()
        second = asyncio.create_task(aio_cache.get_object(refs[1]))
        await started[1].wait()
        release[0].set()
        assert await first == 0
        release[1].set()
        assert await second == 1
        assert calls == [1, 1]

    try:
        asyncio.run(run())
    finally:
        for ref in refs:
            cache_module._cache_remove((ref.endpoint, ref.key))


def test_failed_uncached_fetch_does_not_retain_invalidation_marker(monkeypatch):
    import gc

    ref = ObjectRef("grpc://cache:1", "audit/session/missing-marker", 1)

    async def fetch(ref, version, uuid=""):
        raise ValueError("missing object")

    monkeypatch.setattr(aio_cache, "_fetch_object_data", fetch)

    async def run():
        try:
            await aio_cache.get_object(ref)
        except ValueError:
            pass

    asyncio.run(run())
    gc.collect()
    assert (ref.endpoint, ref.key) not in cache_module._cache_tokens


def test_invalid_package_source_is_rejected_before_upload(monkeypatch, tmp_path):
    uploaded = []

    async def upload(*args, **kwargs):
        uploaded.append(args)

    monkeypatch.setattr(aio_cache, "_upload_file", upload)

    async def run():
        with pytest.raises((ValueError, IsADirectoryError)):
            await aio_cache.upload_package("audit", str(tmp_path), "package.tar")

    asyncio.run(run())
    assert uploaded == []


def test_package_repeated_name_appends_and_requires_a_new_name_for_single_part_download(monkeypatch, tmp_path):
    """Repeated PUT preserves the first part and appends; package downloads accept only one part."""
    source = tmp_path / "package.tar.gz"
    source.write_bytes(b"first package")
    stored = {}

    class Client:
        async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
            frames = [frame async for frame in requests]
            key = frames[0].header.key
            payload = b"".join(frame.data for frame in frames[1:])
            assert frames[0].header.size == len(payload)
            stored.setdefault(key, []).append(payload)
            return SimpleNamespace(endpoint="grpc://cache:1", key=key, version=len(stored[key]))

        def Get(self, request):  # noqa: N802 - gRPC method
            async def responses():
                values = stored[request.key]
                yield cache_pb2.CacheGetResponse(
                    header=cache_pb2.CacheGetHeader(
                        metadata=cache_pb2.CacheObjectMetadata(
                            key=request.key,
                            uuid="package-object",
                            version=len(values),
                            data_type="raw",
                        )
                    )
                )
                for part, payload in enumerate(values):
                    yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=part, data=payload))

            return responses()

    monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: Client())

    async def run():
        first = await aio_cache.upload_package("audit", str(source), endpoint="grpc://cache:1")
        source.write_bytes(b"second package")
        appended = await aio_cache.upload_package("audit", str(source), endpoint="grpc://cache:1")
        assert appended.key == first.key
        assert (first.version, appended.version) == (1, 2)
        assert stored[first.key] == [b"first package", b"second package"]
        rejected = tmp_path / "multipart-output"
        with pytest.raises(ValueError, match="first version only"):
            await aio_cache.download_package("audit", source.name, str(rejected), endpoint="grpc://cache:1")
        assert not rejected.exists()
        fresh = await aio_cache.upload_package("audit", str(source), "another.tar.gz", endpoint="grpc://cache:1")
        assert fresh.version == 1
        assert stored[fresh.key] == [b"second package"]
        downloaded = tmp_path / "single-part-output"
        await aio_cache.download_package("audit", "another.tar.gz", str(downloaded), endpoint="grpc://cache:1")
        assert downloaded.read_bytes() == b"second package"

    asyncio.run(run())


@pytest.mark.parametrize("payload", [b"", b"opaque bytes", pytest.param("numpy", id="compressed-numpy")])
def test_write_headers_declare_exact_encoded_put_and_patch_sizes(monkeypatch, payload):
    if payload == "numpy":
        payload = pytest.importorskip("numpy").arange(1024, dtype="int64")
    endpoint = "grpc://cache:1"
    monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))
    written = []

    class Client:
        async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
            frames = [frame async for frame in requests]
            header = frames[0].header
            wire = b"".join(frame.data for frame in frames[1:])
            assert header.size == len(wire)
            written.append((header, wire))
            return SimpleNamespace(endpoint=endpoint, key="audit/session/object", version=len(written))

        async def Patch(self, requests, timeout=None):  # noqa: N802 - gRPC method
            return await self.Put(requests, timeout)

        def Get(self, request):  # noqa: N802 - gRPC method
            async def responses():
                yield cache_pb2.CacheGetResponse(
                    header=cache_pb2.CacheGetHeader(
                        metadata=cache_pb2.CacheObjectMetadata(
                            key=request.key,
                            uuid="object",
                            version=1,
                            data_type=written[0][0].data_type,
                        )
                    )
                )

            return responses()

    monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: Client())

    async def run():
        ref = await aio_cache.put_object("audit/session", payload)
        await aio_cache.patch_object(ref, payload)

    asyncio.run(run())
    assert len(written) == 2
    assert written[0][0].size == written[1][0].size
    assert written[0][1] == written[1][1]
    if not isinstance(payload, bytes):
        assert written[0][0].data_type.endswith(".zstd")
        assert written[0][0].size != payload.nbytes


def test_empty_file_upload_declares_zero_and_sends_no_payload_chunks(monkeypatch, tmp_path):
    source = tmp_path / "empty.tar"
    source.touch()
    headers = []

    class Client:
        async def Put(self, requests, timeout=None):  # noqa: N802 - gRPC method
            frames = [frame async for frame in requests]
            assert len(frames) == 1
            headers.append(frames[0].header)
            return SimpleNamespace(endpoint="grpc://cache:1", key=frames[0].header.key, version=1)

    monkeypatch.setattr(aio_cache, "_get_client", lambda endpoint, tls=None: Client())
    asyncio.run(aio_cache.upload_package("audit", str(source), endpoint="grpc://cache:1"))
    assert headers[0].size == 0
