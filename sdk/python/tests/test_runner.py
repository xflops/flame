from flamepy.runner.storage import CacheStorage, FileStorage, create_storage_backend


def test_file_storage_basic(tmp_path):
    storage_dir = tmp_path
    base = f"file://{storage_dir}"
    fs = FileStorage(base)
    local = tmp_path / "pkg.txt"
    local.write_text("data")
    url = fs.upload(str(local), "pkg.txt")
    assert url.startswith("file://")
    dest = tmp_path / "out.txt"
    fs.download("pkg.txt", str(dest))
    assert dest.exists()


def test_create_storage_backend_factory(tmp_path):
    path = tmp_path / "storage"
    path.mkdir()
    back = create_storage_backend(f"file://{path}")
    assert isinstance(back, FileStorage)


def test_create_storage_backend_grpc():
    back = create_storage_backend("grpc://localhost:9090", app_name="myapp")
    assert isinstance(back, CacheStorage)


def test_create_storage_backend_grpcs():
    back = create_storage_backend("grpcs://localhost:9090", app_name="myapp")
    assert isinstance(back, CacheStorage)


def test_create_storage_backend_default_uses_cache(monkeypatch):
    from flamepy.core.types import FlameClientCache

    class MockContext:
        cache = FlameClientCache(endpoint="grpc://host:9090")

    monkeypatch.setattr("flamepy.core.types.FlameContext", lambda: MockContext())

    back = create_storage_backend(None, app_name="myapp")
    assert isinstance(back, CacheStorage)


class TestCacheStorage:
    def test_upload(self, monkeypatch, tmp_path):
        from flamepy.core.cache import ObjectRef

        test_file = tmp_path / "myapp-1.0.0.tar.gz"
        test_file.write_bytes(b"package content")

        def mock_upload_object(key, file_path):
            assert key == "myapp/pkg/myapp-1.0.0.tar.gz"
            return ObjectRef(endpoint="grpc://host:9090", key=key, version=1)

        monkeypatch.setattr("flamepy.core.cache.upload_object", mock_upload_object)

        storage = CacheStorage("grpc://host:9090", app_name="myapp")
        url = storage.upload(str(test_file), "myapp-1.0.0.tar.gz")

        assert url == "grpc://host:9090/myapp/pkg/myapp-1.0.0.tar.gz"

    def test_download(self, monkeypatch, tmp_path):
        dest_file = tmp_path / "downloaded.tar.gz"

        def mock_download_object(ref, dest_path):
            assert ref.key == "myapp/pkg/myapp-1.0.0.tar.gz"
            with open(dest_path, "wb") as f:
                f.write(b"downloaded content")

        monkeypatch.setattr("flamepy.core.cache.download_object", mock_download_object)

        storage = CacheStorage("grpc://host:9090", app_name="myapp")
        storage.download("myapp-1.0.0.tar.gz", str(dest_file))

        assert dest_file.exists()
        assert dest_file.read_bytes() == b"downloaded content"

    def test_delete(self, monkeypatch):
        deleted_key = None

        def mock_delete_objects(key):
            nonlocal deleted_key
            deleted_key = key

        monkeypatch.setattr("flamepy.core.cache.delete_objects", mock_delete_objects)

        storage = CacheStorage("grpc://host:9090", app_name="myapp")
        storage.delete("myapp-1.0.0.tar.gz")

        assert deleted_key == "myapp/pkg/myapp-1.0.0.tar.gz"

    def test_upload_requires_app_name(self, tmp_path):
        import pytest

        test_file = tmp_path / "test.tar.gz"
        test_file.write_bytes(b"content")

        storage = CacheStorage("grpc://host:9090", app_name=None)

        with pytest.raises(Exception, match="app_name is required"):
            storage.upload(str(test_file), "test.tar.gz")
