"""Tests for flamepy app APIs."""

import sys
import tarfile
import threading
from concurrent.futures import Future
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import cloudpickle
import pytest

import flamepy.app.client as app_client
from flamepy.app.client import (
    _NoopApplicationOwner,
    _NoopSessionOwner,
    _RuntimeApplicationOwner,
    _RuntimeState,
    _ServiceSessionOwner,
    _ServiceState,
)
from flamepy.app.client import _Runtime as Runtime
from flamepy.app.storage import CacheStorage, FileStorage, create_storage_backend
from flamepy.app.types import ServiceContext, ServiceRequest
from flamepy.core.types import ApplicationState, FlameError, FlameErrorCode, Shim

# App Storage Tests


def _parse_toml(content: str):
    if sys.version_info < (3, 11):
        return None

    import tomllib

    return tomllib.loads(content)


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


def test_create_storage_backend_grpcs_proxy():
    back = create_storage_backend("grpcs-proxy://gateway.example:8080", app_name="myapp")
    assert isinstance(back, CacheStorage)
    assert back._endpoint == "grpcs-proxy://gateway.example:8080"


def test_create_storage_backend_default_uses_cache(monkeypatch):
    from flamepy.core.types import FlameClientCache

    class MockContext:
        cache = FlameClientCache(endpoint="grpc://host:9090")

    monkeypatch.setattr("flamepy.core.types.FlameContext", lambda: MockContext())

    back = create_storage_backend(None, app_name="myapp")
    assert isinstance(back, CacheStorage)


def test_app_application_inherits_template_shim(monkeypatch, tmp_path):
    context = SimpleNamespace(
        package=SimpleNamespace(storage=f"file://{tmp_path}"),
        cache=None,
        app="flmrun",
    )
    template = SimpleNamespace(
        shim=Shim.CRI,
        image="registry.example/flmrt:latest",
        command="flmrun-service",
        working_directory=None,
        environments=None,
        labels=None,
        arguments=None,
        max_instances=None,
        delay_release=None,
        schema=None,
        installer=None,
    )
    storage = MagicMock()
    registered = MagicMock()

    monkeypatch.setattr("flamepy.app.client.FlameContext", lambda: context)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.get_application",
        MagicMock(side_effect=[None, template]),
    )
    monkeypatch.setattr("flamepy.app.client.create_storage_backend", lambda *args, **kwargs: storage)
    monkeypatch.setattr(Runtime, "_create_package", lambda self: str(tmp_path / "app.tar.gz"))
    monkeypatch.setattr(Runtime, "_upload_package", lambda self: "grpc://cache/app.tar.gz")
    monkeypatch.setattr("flamepy.app.client.core_client.register_application", registered)
    open_session = MagicMock()
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)
    runtime = Runtime("generated-app")

    attributes = registered.call_args.args[1]
    assert attributes.shim == Shim.CRI
    assert attributes.labels is None
    assert runtime._state is _RuntimeState.ACTIVE
    assert isinstance(runtime._application_owner, _RuntimeApplicationOwner)
    open_session.assert_not_called()


def test_runtime_reuses_existing_application_with_noop_owner(monkeypatch):
    context = SimpleNamespace(package=None, cache=None, app="flmrun")
    register_application = MagicMock()
    unregister_application = MagicMock()
    open_session = MagicMock()

    monkeypatch.setattr("flamepy.app.client.FlameContext", lambda: context)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.get_application",
        MagicMock(return_value=SimpleNamespace(state=ApplicationState.ENABLED, labels=["external"], url=None)),
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.register_application",
        register_application,
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.unregister_application",
        unregister_application,
    )
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)

    runtime = Runtime("existing-app")

    assert runtime._state is _RuntimeState.ACTIVE
    assert isinstance(runtime._application_owner, _NoopApplicationOwner)
    register_application.assert_not_called()
    open_session.assert_not_called()

    runtime.close()

    assert runtime._state is _RuntimeState.INACTIVE
    unregister_application.assert_not_called()


def test_runtime_rejects_existing_disabled_application(monkeypatch):
    context = SimpleNamespace(package=None, cache=None, app="flmrun")
    register_application = MagicMock()

    monkeypatch.setattr("flamepy.app.client.FlameContext", lambda: context)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.get_application",
        MagicMock(return_value=SimpleNamespace(state=ApplicationState.DISABLED)),
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.register_application",
        register_application,
    )

    with pytest.raises(FlameError, match="Application 'disabled-app' is disabled") as error:
        Runtime("disabled-app")

    assert error.value.code == FlameErrorCode.INVALID_STATE
    register_application.assert_not_called()


def test_registration_race_cleans_only_attempt_package(monkeypatch, tmp_path):
    context = SimpleNamespace(
        package=SimpleNamespace(storage=f"file://{tmp_path}"),
        cache=None,
        app="flmrun",
    )
    template = SimpleNamespace(
        shim=Shim.CRI,
        image=None,
        command="python",
        working_directory=None,
        environments=None,
        labels=None,
        arguments=None,
        max_instances=None,
        delay_release=None,
        schema=None,
        installer=None,
    )
    losing_package = tmp_path / "loser.tar.gz"
    losing_package.write_bytes(b"loser")
    storage = MagicMock()
    storage.upload.return_value = f"file://{losing_package}"
    monkeypatch.setattr("flamepy.app.client.FlameContext", lambda: context)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.get_application",
        MagicMock(side_effect=[None, template]),
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.register_application",
        MagicMock(side_effect=FlameError(FlameErrorCode.INTERNAL, "application already exists")),
    )
    monkeypatch.setattr("flamepy.app.client.create_storage_backend", lambda *args, **kwargs: storage)
    monkeypatch.setattr(Runtime, "_create_package", lambda self: str(losing_package))

    with pytest.raises(FlameError, match="application already exists"):
        Runtime("racing-app")

    storage.delete.assert_called_once_with("loser.tar.gz")


class TestCacheStorage:
    def test_upload(self, monkeypatch, tmp_path):
        from flamepy.core.cache import ObjectRef

        test_file = tmp_path / "myapp-1.0.0.tar.gz"
        test_file.write_bytes(b"package content")

        def mock_upload_object(key, file_path, endpoint=None):
            assert key == "myapp/pkg/myapp-1.0.0.tar.gz"
            assert endpoint == "grpc://host:9090"
            return ObjectRef(endpoint="grpc://host:9090", key=key, version=1)

        monkeypatch.setattr("flamepy.core.cache.upload_object", mock_upload_object)

        storage = CacheStorage("grpc://host:9090", app_name="myapp")
        url = storage.upload(str(test_file), "myapp-1.0.0.tar.gz")

        assert url == "grpc://host:9090/myapp/pkg/myapp-1.0.0.tar.gz"

    def test_upload_preserves_returned_cache_endpoint(self, monkeypatch, tmp_path):
        from flamepy.core.cache import ObjectRef

        test_file = tmp_path / "myapp-1.0.0.tar.gz"
        test_file.write_bytes(b"package content")

        def mock_upload_object(key, file_path, endpoint=None):
            assert endpoint == "grpcs-proxy://gateway.example:443"
            return ObjectRef(endpoint="grpc://10.0.0.42:9090", key=key, version=1)

        monkeypatch.setattr("flamepy.core.cache.upload_object", mock_upload_object)

        storage = CacheStorage("grpcs-proxy://gateway.example:443", app_name="myapp")
        url = storage.upload(str(test_file), "myapp-1.0.0.tar.gz")

        assert url == "grpc://10.0.0.42:9090/myapp/pkg/myapp-1.0.0.tar.gz"

    def test_upload_converts_flight_tls_scheme_for_package_url(self, monkeypatch, tmp_path):
        from flamepy.core.cache import ObjectRef

        test_file = tmp_path / "myapp-1.0.0.tar.gz"
        test_file.write_bytes(b"package content")

        def mock_upload_object(key, file_path, endpoint=None):
            assert endpoint == "grpcs://cache-service:9090"
            return ObjectRef(endpoint="grpc+tls://10.0.0.42:9090", key=key, version=1)

        monkeypatch.setattr("flamepy.core.cache.upload_object", mock_upload_object)

        storage = CacheStorage("grpcs://cache-service:9090", app_name="myapp")
        url = storage.upload(str(test_file), "myapp-1.0.0.tar.gz")

        assert url == "grpcs://10.0.0.42:9090/myapp/pkg/myapp-1.0.0.tar.gz"

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


# App Service Tests


def test_app_service_instance_is_the_only_public_service_type():
    import flamepy.app as app
    from flamepy.app import ServiceInstance
    from flamepy.app import ServiceInstance as ProxyType

    assert ServiceInstance is ProxyType
    assert not hasattr(app, "Service")


def test_app_exports_use_the_package_implementation():
    import flamepy
    import flamepy.app as app

    assert not hasattr(flamepy, "App")
    assert not hasattr(flamepy, "init")
    assert not callable(flamepy.service)
    assert not hasattr(flamepy, "destroy")
    assert callable(app.init)
    assert callable(app.service)
    assert callable(app.destroy)
    assert callable(app.put)
    assert app.service is app_client.service
    assert app.put is app_client.put
    assert not hasattr(app, "put_object")
    assert not hasattr(app, "get_data")
    assert not hasattr(app, "Error")
    assert not hasattr(app, "ErrorType")
    assert not hasattr(app, "_Runtime")
    assert callable(app._restore_service_instance)
    assert not hasattr(flamepy, "FlameServiceContext")
    assert not hasattr(flamepy, "FlameRunnerContext")


def test_process_app_lifecycle_is_idempotent_and_rejects_a_different_app(monkeypatch):
    import flamepy.app as app
    from flamepy import FlameError

    created = []

    class FakeRuntime:
        def __init__(self, name, **kwargs):
            self._name = name
            self.kwargs = kwargs
            self.closed = 0
            created.append(self)

        def close(self):
            assert app_client._runtime is self
            self.closed += 1

    monkeypatch.setattr(app_client, "_Runtime", FakeRuntime)
    monkeypatch.setattr(app_client, "_runtime", None)
    rr = app.init("shared-app", dependencies=["numpy"])
    assert rr is created[0]
    assert app.init("shared-app") is rr
    assert len(created) == 1

    with pytest.raises(FlameError, match="already initialized"):
        app.init("other-app")

    assert app.destroy() is None
    assert app.destroy() is None
    assert created[0].closed == 1
    assert app_client._runtime is None


def test_process_app_init_replaces_a_directly_closed_runtime(monkeypatch):
    import flamepy.app as app

    created = []

    class FakeRuntime:
        def __init__(self, name, **kwargs):
            self._name = name
            self._state = _RuntimeState.ACTIVE
            created.append(self)

        def close(self):
            self._state = _RuntimeState.INACTIVE

    monkeypatch.setattr(app_client, "_Runtime", FakeRuntime)
    monkeypatch.setattr(app_client, "_runtime", None)

    first = app.init("restartable-app")
    first.close()
    second = app.init("restartable-app")

    assert second is not first
    assert second._state is _RuntimeState.ACTIVE
    assert created == [first, second]


def test_module_facade_supports_cross_module_service_declarations(monkeypatch):
    import types

    import flamepy.app as app

    declarations = []

    class FakeRuntime:
        def __init__(self, name, **kwargs):
            self._name = name
            declarations.append(("runtime", name))

        def service(self, **options):
            assert app_client._runtime is self

            def decorator(execution_object):
                declarations.append((execution_object.__module__, execution_object.__name__, options))
                return execution_object

            return decorator

        def close(self):
            pass

    monkeypatch.setattr(app_client, "_Runtime", FakeRuntime)
    monkeypatch.setattr(app_client, "_runtime", None)
    service_module = types.ModuleType("cross_package.services")

    exec(
        "import flamepy.app as app\napp.init('shared-app')\n@app.service(resreq='cpu=1')\ndef fn_a(value):\n    return value * 2\ncaptured_fn_a = fn_a\n@app.service()\ndef fn_b(value):\n    return captured_fn_a(value) + 1\n",
        service_module.__dict__,
    )

    assert app.init("shared-app") is app_client._runtime
    assert declarations == [
        ("runtime", "shared-app"),
        (
            "cross_package.services",
            "fn_a",
            {"autoscale": None, "warmup": 0, "resreq": "cpu=1"},
        ),
        (
            "cross_package.services",
            "fn_b",
            {"autoscale": None, "warmup": 0, "resreq": None},
        ),
    ]


def test_module_service_decorator_is_supported_after_init(monkeypatch):
    import flamepy.app as app

    runtime = MagicMock()
    runtime._name = "shared-app"
    runtime.service.return_value = lambda execution_object: execution_object
    monkeypatch.setattr(app_client, "_Runtime", lambda name, **kwargs: runtime)
    monkeypatch.setattr(app_client, "_runtime", None)

    assert app.init("shared-app") is runtime

    @app.service()
    def calculate(value):
        return value * 2

    assert calculate(3) == 6
    runtime.service.assert_called_once_with(autoscale=None, warmup=0, resreq=None)


def test_module_put_uses_the_active_runtime(monkeypatch):
    import flamepy.app as app

    runtime = MagicMock()
    runtime.put.return_value = "object-ref"
    monkeypatch.setattr(app_client, "_runtime", runtime)

    assert app.put({"weights": [1, 2]}) == "object-ref"
    runtime.put.assert_called_once_with({"weights": [1, 2]})


def test_service_module_execution_object_is_pickled_by_value(monkeypatch):
    import types

    from flamepy.app import ServiceInstance

    module_name = "cross_package.pickled_services"
    service_module = types.ModuleType(module_name)
    exec("def calculate(value):\n    return value * 2\n", service_module.__dict__)
    monkeypatch.setitem(sys.modules, module_name, service_module)
    captured = {}

    def put_context(key_prefix, serialized):
        captured["serialized"] = serialized
        return MagicMock(key="context", version=1, encode=MagicMock(return_value=b"context"))

    monkeypatch.setattr("flamepy.app.client._core_put_object", put_context)
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", MagicMock(return_value=MagicMock(id="session")))

    ServiceInstance("shared-app", service_module.calculate)
    monkeypatch.delitem(sys.modules, module_name)
    restored = cloudpickle.loads(captured["serialized"])

    assert restored.execution_object(4) == 8


def test_service_proxy_captured_by_service_reopens_existing_session(monkeypatch):
    from flamepy.app import ObjectFuture, ServiceInstance

    serialized_contexts = []

    def put_context(key_prefix, serialized):
        serialized_contexts.append(serialized)
        return MagicMock(key="context", version=1, encode=MagicMock(return_value=b"context"))

    fn_a_session = MagicMock(id="fn-a-session")
    fn_b_session = MagicMock(id="fn-b-session")
    reopened_fn_a_session = MagicMock(id="fn-a-session")
    nested_future = Future()
    nested_future.set_result(b"result")
    reopened_fn_a_session.run.return_value = nested_future
    open_session = MagicMock(side_effect=[fn_a_session, fn_b_session, reopened_fn_a_session])
    monkeypatch.setattr("flamepy.app.client._core_put_object", put_context)
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)

    def fn_a(value):
        return value * 2

    fn_a = ServiceInstance("shared-app", fn_a)

    def fn_b(value):
        return fn_a(value)

    ServiceInstance("shared-app", fn_b)
    restored_context = cloudpickle.loads(serialized_contexts[1])
    result = restored_context.execution_object(3)

    assert isinstance(result, ObjectFuture)
    open_session.assert_called_with(session_id="fn-a-session")
    reopened_fn_a_session.run.assert_called_once()
    captured_proxy = restored_context.execution_object.__closure__[0].cell_contents
    assert isinstance(captured_proxy._session_owner, _NoopSessionOwner)
    captured_proxy.close()
    reopened_fn_a_session.close.assert_not_called()


def test_runtime_closes_sessions_when_reusing_an_existing_application(monkeypatch):
    unregister_application = MagicMock()
    monkeypatch.setattr(
        "flamepy.app.client.core_client.unregister_application",
        unregister_application,
    )

    runtime = object.__new__(Runtime)
    runtime._name = "existing-app"
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    application_owner = _NoopApplicationOwner()
    runtime._application_owner = application_owner
    services = [MagicMock(), MagicMock()]
    runtime._services = services

    runtime.close()

    for service in services:
        service.close.assert_called_once_with()
    assert runtime._services == []
    assert runtime._state is _RuntimeState.INACTIVE
    assert runtime._application_owner is application_owner
    unregister_application.assert_not_called()


def test_process_app_helpers_require_initialization(monkeypatch):
    import flamepy.app as app
    from flamepy import FlameError

    monkeypatch.setattr(app_client, "_runtime", None)

    with pytest.raises(FlameError, match="flamepy.app.init"):

        @app.service()
        def service_without_init():
            return None


def test_recursive_service_declaration_reuses_context_without_init(monkeypatch):
    import flamepy.app as app
    from flamepy import FlameError
    from flamepy.app._context import _bind_invocation_context
    from flamepy.core.service import ApplicationContext, SessionContext

    monkeypatch.setattr(app_client, "_runtime", None)
    put_context = MagicMock()
    monkeypatch.setattr("flamepy.app.client._core_put_object", put_context)
    session = MagicMock(id="recursive-session")
    pending = MagicMock(spec=Future)
    session.run.return_value = pending
    open_session = MagicMock(return_value=session)
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)
    session_context = SessionContext(None, "recursive-session", ApplicationContext("recursive-app"))

    with _bind_invocation_context(session_context):

        @app.service()
        class RecursiveService:
            def invoke(self):
                return None

        recursive_service = RecursiveService()

    assert recursive_service._app == "recursive-app"
    assert recursive_service._session is session
    assert recursive_service._session_context is session_context
    assert "_session_context" not in recursive_service._execution_object.__dict__
    put_context.assert_not_called()
    open_session.assert_called_once_with(session_id="recursive-session")
    recursive_service.invoke()
    session.run.assert_called_once()
    recursive_service.close()
    pending.result.assert_called_once_with()
    session.close.assert_not_called()
    with pytest.raises(FlameError, match="closed"):
        recursive_service.invoke()


def test_recursive_service_declaration_prefers_invocation_context(monkeypatch):
    import flamepy.app as app
    from flamepy.app._context import _bind_invocation_context
    from flamepy.core.service import ApplicationContext, SessionContext

    runtime = MagicMock()
    monkeypatch.setattr(app_client, "_runtime", runtime)
    session = MagicMock(id="recursive-session")
    open_session = MagicMock(return_value=session)
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)
    session_context = SessionContext(None, "recursive-session", ApplicationContext("recursive-app"))

    with _bind_invocation_context(session_context):

        @app.service()
        class RecursiveService:
            def invoke(self):
                return None

        recursive_service = RecursiveService()

    runtime.service.assert_not_called()
    open_session.assert_called_once_with(session_id="recursive-session")
    assert recursive_service._session_context is session_context
    recursive_service.close()
    session.close.assert_not_called()


@pytest.mark.parametrize(
    "options",
    [
        {"autoscale": True},
        {"warmup": 1},
        {"resreq": "cpu=1"},
    ],
)
def test_recursive_service_declaration_rejects_session_options(monkeypatch, options):
    import flamepy.app as app
    from flamepy import FlameError
    from flamepy.app._context import _bind_invocation_context
    from flamepy.core.service import ApplicationContext, SessionContext

    monkeypatch.setattr(app_client, "_runtime", None)
    open_session = MagicMock()
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)
    session_context = SessionContext(None, "recursive-session", ApplicationContext("recursive-app"))

    with _bind_invocation_context(session_context):
        with pytest.raises(FlameError, match="Nested services reuse the current session"):

            @app.service(**options)
            def recursive_service():
                return None

    open_session.assert_not_called()


def test_recursive_service_declaration_requires_existing_session(monkeypatch):
    import flamepy.app as app
    from flamepy.app._context import _bind_invocation_context
    from flamepy.core.service import ApplicationContext, SessionContext

    monkeypatch.setattr(app_client, "_runtime", None)
    put_context = MagicMock()
    monkeypatch.setattr("flamepy.app.client._core_put_object", put_context)
    open_session = MagicMock(side_effect=RuntimeError("missing session"))
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)
    session_context = SessionContext(None, "missing-session", ApplicationContext("recursive-app"))

    with _bind_invocation_context(session_context):
        with pytest.raises(RuntimeError, match="missing session"):

            @app.service()
            def recursive_service():
                return None

    put_context.assert_not_called()
    open_session.assert_called_once_with(session_id="missing-session")


class DummyObjectRef:
    """Mock ObjectRef for testing."""

    def __init__(self, data=b"ref-data"):
        self._data = data

    @classmethod
    def decode(cls, data: bytes) -> "DummyObjectRef":
        return cls(data)

    def encode(self) -> bytes:
        return self._data


def test_objectfuture_ref_decodes_bytes():
    """Test ObjectFuture.ref() decodes bytes to ObjectRef."""
    from flamepy.app import ObjectFuture

    future = Future()
    future.set_result(b"encoded-ref")

    with patch("flamepy.app.client.ObjectRef", DummyObjectRef):
        of = ObjectFuture(future)
        ref = of.ref()
        assert isinstance(ref, DummyObjectRef)
        assert ref._data == b"encoded-ref"


def test_objectfuture_ref_returns_existing_objectref():
    """Test ObjectFuture.ref() returns ObjectRef if already decoded."""
    from flamepy.app import ObjectFuture

    dummy_ref = DummyObjectRef(b"already-ref")
    future = Future()
    future.set_result(dummy_ref)

    with patch("flamepy.app.client.ObjectRef", DummyObjectRef):
        of = ObjectFuture(future)
        ref = of.ref()
        assert ref is dummy_ref


def test_objectfuture_get_retrieves_object():
    """Test ObjectFuture.get() retrieves actual object from cache."""
    from flamepy.app import ObjectFuture

    future = Future()
    future.set_result(b"encoded-ref")

    with patch("flamepy.app.client.ObjectRef", DummyObjectRef):
        with patch("flamepy.app.client.get_object", return_value={"key": "value"}):
            of = ObjectFuture(future)
            result = of.get()
            assert result == {"key": "value"}


def test_objectfuture_wait_blocks_until_done():
    """Test ObjectFuture.wait() blocks until future completes."""
    from flamepy.app import ObjectFuture

    future = Future()
    future.set_result(b"done")

    of = ObjectFuture(future)
    of.wait()


def test_objectfuture_iterator_yields_in_completion_order():
    """Test ObjectFutureIterator yields futures as they complete."""
    from flamepy.app import ObjectFuture, ObjectFutureIterator

    f1 = Future()
    f2 = Future()
    f1.set_result(b"first")
    f2.set_result(b"second")

    of1 = ObjectFuture(f1)
    of2 = ObjectFuture(f2)

    iterator = ObjectFutureIterator([of1, of2])
    results = list(iterator)

    assert len(results) == 2
    assert of1 in results
    assert of2 in results


def test_app_should_exclude_matches_patterns():
    """Test App._should_exclude() matches exclusion patterns."""
    from flamepy.app.client import _Runtime as Runtime

    app = object.__new__(Runtime)

    assert app._should_exclude("__pycache__", ["__pycache__"])
    assert app._should_exclude("test.pyc", ["*.pyc"])
    assert app._should_exclude(".git", [".git", ".venv"])
    assert not app._should_exclude("main.py", ["*.pyc", "__pycache__"])


def test_app_should_exclude_handles_nested_paths():
    """Test App._should_exclude() handles nested path patterns."""
    from flamepy.app.client import _Runtime as Runtime

    app = object.__new__(Runtime)

    assert app._should_exclude("src/__pycache__/module.pyc", ["*.pyc"])
    assert app._should_exclude("tests/data/file.tmp", ["*.tmp"])


def test_app_package_generates_metadata_without_dependencies(tmp_path, monkeypatch):
    """App packages ad hoc script directories as installable Python projects."""
    from flamepy.app.client import _Runtime as Runtime

    monkeypatch.chdir(tmp_path)
    (tmp_path / "script.py").write_text("def test_fn(x):\n    return x * x\n")

    app = object.__new__(Runtime)
    app._name = "test-run"
    app._dependencies = None
    app._context = SimpleNamespace(package=None)

    package_path = app._create_package()

    assert not (tmp_path / "pyproject.toml").exists()
    with tarfile.open(package_path, "r:gz") as package:
        assert package.getmember("pyproject.toml").mode == 0o644
        pyproject = package.extractfile("pyproject.toml")
        assert pyproject is not None
        content = pyproject.read().decode("utf-8")

    assert 'name = "test-run"' in content
    assert "dependencies = []" in content
    assert "py-modules = []" in content
    parsed = _parse_toml(content)
    if parsed is not None:
        assert parsed["project"]["dependencies"] == []


def test_app_package_generates_dependency_metadata_in_archive(tmp_path, monkeypatch):
    """Generated dependency metadata is packaged without mutating the caller cwd."""
    from flamepy.app.client import _Runtime as Runtime

    monkeypatch.chdir(tmp_path)
    (tmp_path / "script.py").write_text("def test_fn(x):\n    return x * x\n")

    app = object.__new__(Runtime)
    app._name = "test-run"
    app._dependencies = ["pandas>=2", "numpy"]
    app._context = SimpleNamespace(package=None)

    package_path = app._create_package()

    assert not (tmp_path / "pyproject.toml").exists()
    with tarfile.open(package_path, "r:gz") as package:
        assert package.getmember("pyproject.toml").mode == 0o644
        pyproject = package.extractfile("pyproject.toml")
        assert pyproject is not None
        content = pyproject.read().decode("utf-8")

    assert '"numpy"' in content
    assert '"pandas>=2"' in content
    assert "py-modules = []" in content
    parsed = _parse_toml(content)
    if parsed is not None:
        assert parsed["project"]["dependencies"] == ["numpy", "pandas>=2"]


@pytest.mark.parametrize("metadata_file", ["setup.py", "setup.cfg"])
def test_app_package_skips_generated_metadata_for_legacy_projects(tmp_path, monkeypatch, caplog, metadata_file):
    """App does not add pyproject.toml over legacy package metadata."""
    from flamepy.app.client import _Runtime as Runtime

    monkeypatch.chdir(tmp_path)
    if metadata_file == "setup.py":
        (tmp_path / metadata_file).write_text("from setuptools import setup\nsetup(name='legacy-app')\n")
    else:
        (tmp_path / metadata_file).write_text("[metadata]\nname = legacy-app\n")

    app = object.__new__(Runtime)
    app._name = "test-run"
    app._dependencies = ["numpy"]
    app._context = SimpleNamespace(package=None)

    caplog.set_level("WARNING", logger="flamepy.app")
    package_path = app._create_package()

    with tarfile.open(package_path, "r:gz") as package:
        names = package.getnames()

    assert metadata_file in names
    assert "pyproject.toml" not in names
    assert "Skipping pyproject.toml generation" in caplog.text


def test_app_service_instance_generates_method_wrappers():
    """Test ServiceInstance generates wrappers for public methods."""
    from flamepy.app import ServiceInstance

    class Calculator:
        def add(self, a, b):
            return a + b

        def multiply(self, a, b):
            return a * b

        def _private(self):
            pass

    calc = Calculator()
    rs = object.__new__(ServiceInstance)
    rs._app = "test-app"
    rs._execution_object = calc
    rs._function_wrapper = None

    mock_session = MagicMock()
    mock_session.run = MagicMock(return_value=Future())
    rs._session = mock_session

    rs._generate_wrappers()

    assert hasattr(rs, "add")
    assert hasattr(rs, "multiply")
    assert not hasattr(rs, "_private")


def test_app_service_instance_callable_for_function():
    """Test ServiceInstance is callable for a function execution object."""
    from flamepy.app import ServiceInstance

    def my_func(x):
        return x * 2

    rs = object.__new__(ServiceInstance)
    rs._app = "test-app"
    rs._execution_object = my_func
    rs._function_wrapper = None

    mock_session = MagicMock()
    f = Future()
    f.set_result(b"result")
    mock_session.run = MagicMock(return_value=f)
    rs._session = mock_session

    rs._generate_wrappers()

    assert rs._function_wrapper is not None
    assert callable(rs)


def test_app_service_instance_not_callable_for_class():
    """Test ServiceInstance raises TypeError for a class service proxy."""
    from flamepy.app import ServiceInstance

    class MyClass:
        def method(self):
            pass

    rs = object.__new__(ServiceInstance)
    rs._app = "test-app"
    rs._execution_object = MyClass
    rs._function_wrapper = None

    mock_session = MagicMock()
    rs._session = mock_session

    rs._generate_wrappers()

    with pytest.raises(TypeError):
        rs()


def test_app_service_instance_close_closes_session():
    """Test ServiceInstance.close() closes the underlying session."""
    from flamepy.app import ServiceInstance

    rs = object.__new__(ServiceInstance)
    rs._app = "test-app"

    mock_session = MagicMock()
    rs._session = mock_session
    rs._session_owner = _ServiceSessionOwner(mock_session)

    rs.close()
    rs.close()

    mock_session.close.assert_called_once()


def test_app_service_instance_close_drains_pending_tasks():
    """Service teardown waits for submitted tasks before closing the session."""
    from flamepy.app import ServiceInstance

    rs = object.__new__(ServiceInstance)
    rs._app = "test-app"
    rs._future_lock = threading.Lock()
    rs._state_changed = threading.Condition(rs._future_lock)
    pending = MagicMock()
    rs._pending_futures = {pending}
    rs._state = _ServiceState.OPEN
    rs._session = MagicMock()
    rs._session_owner = _ServiceSessionOwner(rs._session)

    rs.close()

    pending.result.assert_called_once_with()
    rs._session.close.assert_called_once_with()
    assert rs._state is _ServiceState.CLOSED


def test_app_service_rejects_method_name_collisions():
    """A service method cannot silently replace a proxy lifecycle method."""
    from flamepy.app import ServiceInstance

    class ServiceWithClose:
        def close(self):
            return "user-close"

    with pytest.raises(TypeError, match="conflict.*close"):
        ServiceInstance("test-app", ServiceWithClose, constructor_args=())


def test_app_service_instance_generates_session_id(monkeypatch):
    from flamepy.app import ServiceInstance

    session = MagicMock(id="generated")
    open_session_mock = MagicMock(return_value=session)
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda *args, **kwargs: MagicMock(key="context", version=1, encode=MagicMock(return_value=b"context")),
    )
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session_mock)

    instance = ServiceInstance("pi-example", lambda: None)

    spec = open_session_mock.call_args.kwargs["spec"]
    assert spec.id.startswith("pi-example-")
    assert isinstance(instance._session_owner, _ServiceSessionOwner)


def test_app_service_instance_ignores_execution_object_session_context(monkeypatch):
    from flamepy.app import ServiceInstance
    from flamepy.core.service import ApplicationContext, SessionContext

    def service():
        return None

    service._session_context = SessionContext(None, "recursive-session", ApplicationContext("pi-example"))
    open_session_mock = MagicMock(return_value=MagicMock(id="recursive-session"))
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda *args, **kwargs: MagicMock(key="context", version=1, encode=MagicMock(return_value=b"context")),
    )
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session_mock)

    instance = ServiceInstance("pi-example", service)

    assert open_session_mock.call_args.kwargs["session_id"].startswith("pi-example-")
    assert isinstance(instance._session_owner, _ServiceSessionOwner)
    instance.close()
    instance._session.close.assert_called_once_with()


def test_runtime_service_owns_session_despite_execution_object_attribute(monkeypatch):
    from flamepy.app.client import _Runtime
    from flamepy.core.service import ApplicationContext, SessionContext

    class RecursiveService:
        _session_context = SessionContext(None, "recursive-session", ApplicationContext("pi-example"))

    session = MagicMock(id="recursive-session")
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda *args, **kwargs: MagicMock(key="context", version=1, encode=MagicMock(return_value=b"context")),
    )
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", MagicMock(return_value=session))

    runtime = object.__new__(_Runtime)
    runtime._name = "pi-example"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    monkeypatch.setattr(app_client, "_runtime", runtime)
    service_factory = runtime.service()(RecursiveService)
    service = service_factory()

    assert isinstance(service._session_owner, _ServiceSessionOwner)
    service.close()
    session.close.assert_called_once_with()


def test_app_context_defaults_by_execution_object():
    """Test ServiceContext defaults for function and class services."""
    import functools

    def sample_func(x=0):
        return x

    class SampleService:
        def method(self):
            return "ok"

    contexts = [
        ServiceContext(sample_func),
        ServiceContext(functools.partial(sample_func, x=1)),
        ServiceContext(len),
        ServiceContext(SampleService, constructor_args=()),
        ServiceContext(
            SampleService,
            constructor_args=(),
            autoscale=False,
            warmup=2,
        ),
    ]

    assert [(ctx.autoscale, ctx.warmup, ctx.min_instances, ctx.max_instances) for ctx in contexts] == [
        (True, 0, 0, None),
        (True, 0, 0, None),
        (True, 0, 0, None),
        (True, 0, 0, None),
        (False, 2, 2, 2),
    ]
    assert [ctx.constructor_args for ctx in contexts] == [
        None,
        None,
        None,
        (),
        (),
    ]


def test_app_service_passes_public_options(monkeypatch):
    """Test the runtime service decorator forwards only public options."""
    from flamepy.app.client import _Runtime as Runtime

    calls = []

    class FakeServiceInstance:
        def __init__(
            self,
            app,
            execution_object,
            autoscale=None,
            warmup=0,
            resreq=None,
            constructor_args=None,
            constructor_kwargs=None,
        ):
            calls.append(
                (
                    app,
                    execution_object,
                    autoscale,
                    warmup,
                    resreq,
                    constructor_args,
                    constructor_kwargs,
                )
            )

    def sample_func():
        return "ok"

    app = object.__new__(Runtime)
    app._name = "test-app-service-options"
    app._services = []
    app._state = _RuntimeState.ACTIVE
    app._lifecycle_lock = threading.RLock()
    resreq = "cpu=1,mem=1g"

    monkeypatch.setattr("flamepy.app.client.ServiceInstance", FakeServiceInstance)

    decorator = app.service(autoscale=False, warmup=2, resreq=resreq)
    decorated = decorator(sample_func)

    assert isinstance(decorated, FakeServiceInstance)

    assert calls == [
        (
            "test-app-service-options",
            sample_func,
            False,
            2,
            resreq,
            None,
            None,
        )
    ]


def test_decorated_class_creates_session_only_when_constructed(monkeypatch):
    from flamepy.app import ServiceInstance

    serialized_contexts = []

    def put_context(key_prefix, serialized):
        serialized_contexts.append(serialized)
        return MagicMock(
            key="context",
            version=1,
            encode=MagicMock(return_value=b"context"),
        )

    session = MagicMock(id="worker-session")
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        put_context,
    )
    open_session = MagicMock(return_value=session)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        open_session,
    )

    class Worker:
        constructions = []

        def __init__(self, name, *, replicas=1):
            self.name = name
            self.replicas = replicas
            type(self).constructions.append((name, replicas))

        def run(self):
            return self.name

    runtime = object.__new__(Runtime)
    runtime._name = "dual-class-app"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    monkeypatch.setattr(app_client, "_runtime", runtime)

    worker_factory = runtime.service(resreq="cpu=2")(Worker)

    assert callable(worker_factory)
    assert worker_factory.__name__ == "Worker"
    assert not hasattr(worker_factory, "run")
    assert Worker.constructions == []
    assert runtime._services == []
    open_session.assert_not_called()

    worker = worker_factory("remote", replicas=3)

    assert isinstance(worker, ServiceInstance)
    assert worker._execution_object is Worker
    assert Worker.constructions == []
    assert runtime._services == [worker]
    assert open_session.call_count == 1

    context = cloudpickle.loads(serialized_contexts[0])
    assert context.constructor_args == ("remote",)
    assert context.constructor_kwargs == {"replicas": 3}


def test_decorated_class_has_no_class_level_remote_methods(monkeypatch):
    class Worker:
        def run(self):
            return "ok"

    runtime = object.__new__(Runtime)
    runtime._name = "no-class-level-service-app"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    monkeypatch.setattr(app_client, "_runtime", runtime)

    worker_factory = runtime.service()(Worker)

    with pytest.raises(AttributeError):
        worker_factory.run()


def test_decorated_class_factory_rejects_calls_after_runtime_is_inactive(
    monkeypatch,
):
    from flamepy import FlameError

    class Worker:
        def run(self):
            return "ok"

    runtime = object.__new__(Runtime)
    runtime._name = "inactive-class-service-app"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    monkeypatch.setattr(app_client, "_runtime", runtime)

    worker_factory = runtime.service()(Worker)
    runtime._state = _RuntimeState.INACTIVE

    with pytest.raises(FlameError, match="declared this service class is not active"):
        worker_factory()


def test_decorated_class_carries_constructor_arguments_without_local_construction(
    monkeypatch,
):
    serialized_contexts = []

    def put_context(key_prefix, serialized):
        serialized_contexts.append(serialized)
        return MagicMock(
            key="context",
            version=1,
            encode=MagicMock(return_value=b"context"),
        )

    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        put_context,
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        MagicMock(return_value=MagicMock(id="worker-session")),
    )

    class Worker:
        constructions = 0

        def __init__(self, name, *, replicas):
            type(self).constructions += 1
            self.name = name
            self.replicas = replicas

        def run(self):
            return None

    runtime = object.__new__(Runtime)
    runtime._name = "invalid-instance-app"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    monkeypatch.setattr(app_client, "_runtime", runtime)
    worker_factory = runtime.service(autoscale=True, warmup=2)(Worker)
    worker_factory("remote", replicas=4)

    assert Worker.constructions == 0
    context = cloudpickle.loads(serialized_contexts[0])
    assert context.execution_object.__name__ == "Worker"
    assert context.constructor_args == ("remote",)
    assert context.constructor_kwargs == {"replicas": 4}
    assert context.autoscale is True
    assert context.warmup == 2
    assert context.min_instances == 2
    assert context.max_instances is None


def test_decorated_class_preserves_resources_and_runtime_closes_all_instances(
    monkeypatch,
):
    sessions = [MagicMock(id="first-session"), MagicMock(id="second-session")]
    open_session = MagicMock(side_effect=sessions)
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda *args, **kwargs: MagicMock(
            key="context",
            version=1,
            encode=MagicMock(return_value=b"context"),
        ),
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        open_session,
    )

    class Worker:
        def run(self):
            return None

    runtime = object.__new__(Runtime)
    runtime._name = "class-lifecycle-app"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    runtime._application_owner = MagicMock()
    monkeypatch.setattr(app_client, "_runtime", runtime)

    worker_factory = runtime.service(resreq="cpu=2,mem=1g")(Worker)

    assert runtime._services == []
    open_session.assert_not_called()

    first_worker = worker_factory()
    second_worker = worker_factory()

    assert runtime._services == [first_worker, second_worker]
    requirements = [call.kwargs["spec"].resreq for call in open_session.call_args_list]
    assert [(item.cpu, item.memory) for item in requirements] == [
        (2, 1024**3),
        (2, 1024**3),
    ]

    runtime.close()

    sessions[0].close.assert_called_once_with()
    sessions[1].close.assert_called_once_with()
    assert runtime._services == []
    runtime._application_owner.unregister.assert_called_once_with()


def test_decorated_class_instance_is_pickled_by_defining_module(monkeypatch):
    import types

    module_name = "cross_package.pickled_class_service"
    service_module = types.ModuleType(module_name)
    exec(
        "class Worker:\n    def __init__(self, value):\n        self.value = value\n    def run(self):\n        return self.value\n",
        service_module.__dict__,
    )
    monkeypatch.setitem(sys.modules, module_name, service_module)
    serialized_contexts = []

    def put_context(key_prefix, serialized):
        serialized_contexts.append(serialized)
        return MagicMock(
            key=f"context-{len(serialized_contexts)}",
            version=1,
            encode=MagicMock(return_value=b"context"),
        )

    monkeypatch.setattr("flamepy.app.client._core_put_object", put_context)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        MagicMock(
            side_effect=[
                MagicMock(id="class-session"),
                MagicMock(id="instance-session"),
            ]
        ),
    )

    runtime = object.__new__(Runtime)
    runtime._name = "pickled-class-app"
    runtime._services = []
    runtime._state = _RuntimeState.ACTIVE
    runtime._lifecycle_lock = threading.RLock()
    monkeypatch.setattr(app_client, "_runtime", runtime)
    original_class = service_module.Worker
    worker_factory = runtime.service()(original_class)
    service_module.Worker = worker_factory

    worker_factory(7)
    monkeypatch.delitem(sys.modules, module_name)
    restored = cloudpickle.loads(serialized_contexts[-1])

    assert restored.execution_object.__name__ == "Worker"
    assert restored.constructor_args == (7,)
    assert restored.constructor_kwargs == {}
    restored_object = restored.execution_object(
        *restored.constructor_args,
        **restored.constructor_kwargs,
    )
    assert restored_object.run() == 7


def test_app_service_rejects_object_instances():
    """The public decorator accepts only functions and classes."""
    from flamepy.app.client import _Runtime as Runtime

    app = object.__new__(Runtime)
    app._name = "test-app-invalid-defaults"
    app._services = []
    app._state = _RuntimeState.ACTIVE

    class CallableObject:
        def __call__(self):
            return None

    for execution_object in (object(), CallableObject()):
        with pytest.raises(TypeError, match="function or class"):
            decorator = app.service()
            decorator(execution_object)


@pytest.mark.parametrize(
    ("execution_object", "kwargs", "message"),
    [
        (lambda: None, {"warmup": -1}, "warmup must be a non-negative integer"),
    ],
)
def test_app_context_rejects_unsupported_options(execution_object, kwargs, message):
    """Test ServiceContext rejects unsupported options."""
    with pytest.raises(ValueError, match=message):
        ServiceContext(execution_object, **kwargs)


def test_app_service_rejects_positional_execution_object():
    app = object.__new__(Runtime)

    with pytest.raises(TypeError, match="positional argument"):
        app.service(lambda: None)


def test_app_service_instance_parses_resource_string(monkeypatch):
    from flamepy.app import ServiceInstance

    open_session_mock = MagicMock(return_value=MagicMock(id="generated"))
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda *args, **kwargs: MagicMock(key="context", version=1, encode=MagicMock(return_value=b"context")),
    )
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session_mock)

    ServiceInstance("resource-app", lambda: None, resreq="cpu=2,mem=1g,gpu=1")

    requirement = open_session_mock.call_args.kwargs["spec"].resreq
    assert (requirement.cpu, requirement.memory, requirement.gpu) == (2, 1024**3, 1)


def test_app_service_instance_rejects_non_string_resources(monkeypatch):
    from flamepy.app import ServiceInstance

    with pytest.raises(TypeError, match="resreq must be a string"):
        ServiceInstance("resource-app", lambda: None, resreq=object())


def test_app_service_rejects_non_string_resources_before_class_construction():
    runtime = object.__new__(Runtime)

    with pytest.raises(TypeError, match="resreq must be a string"):
        runtime.service(resreq=object())


def test_app_service_instance_serializes_partial_from_defining_module(monkeypatch):
    import functools

    from flamepy.app import ServiceInstance

    serialized_contexts = []

    def add(left, right):
        return left + right

    execution_object = functools.partial(add, 4)
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda key, value: serialized_contexts.append(value) or MagicMock(encode=MagicMock(return_value=b"context")),
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        MagicMock(return_value=MagicMock(id="partial-session")),
    )

    ServiceInstance("partial-app", execution_object)

    restored = cloudpickle.loads(serialized_contexts[0])
    assert restored.execution_object(5) == 9
    assert app_client._execution_module(execution_object) is sys.modules[__name__]


def test_app_service_serializes_by_value_under_process_wide_lock(monkeypatch):
    """Concurrent declarations cannot overlap cloudpickle registry mutations."""
    from flamepy.app import ServiceInstance

    first_dump_entered = threading.Event()
    release_first_dump = threading.Event()
    metric_lock = threading.Lock()
    active_registrations = 0
    maximum_registrations = 0
    dump_calls = 0
    errors = []

    def execution_object():
        return None

    def register(_module):
        nonlocal active_registrations, maximum_registrations
        with metric_lock:
            active_registrations += 1
            maximum_registrations = max(
                maximum_registrations,
                active_registrations,
            )

    def unregister(_module):
        nonlocal active_registrations
        with metric_lock:
            active_registrations -= 1

    def serialize(_context, protocol):
        nonlocal dump_calls
        with metric_lock:
            dump_calls += 1
            call_number = dump_calls
        if call_number == 1:
            first_dump_entered.set()
            assert release_first_dump.wait(timeout=5)
        return b"context"

    monkeypatch.setattr(cloudpickle, "list_registry_pickle_by_value", lambda: set())
    monkeypatch.setattr(cloudpickle, "register_pickle_by_value", register)
    monkeypatch.setattr(cloudpickle, "unregister_pickle_by_value", unregister)
    monkeypatch.setattr(cloudpickle, "dumps", serialize)
    monkeypatch.setattr(
        "flamepy.app.client._core_put_object",
        lambda *args: MagicMock(encode=MagicMock(return_value=b"context-ref")),
    )
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        lambda **kwargs: MagicMock(id=kwargs["session_id"]),
    )

    def declare_service():
        try:
            ServiceInstance("concurrent-app", execution_object)
        except Exception as error:
            errors.append(error)

    first = threading.Thread(target=declare_service)
    second = threading.Thread(target=declare_service)
    first.start()
    assert first_dump_entered.wait(timeout=5)
    second.start()
    release_first_dump.set()
    first.join(timeout=5)
    second.join(timeout=5)

    assert not first.is_alive()
    assert not second.is_alive()
    assert errors == []
    assert dump_calls == 2
    assert maximum_registrations == 1
    assert active_registrations == 0


def test_app_service_validates_combined_publication_limit():
    import flamepy.app as app
    from flamepy.app._context import _bind_invocation_context
    from flamepy.core.service import ApplicationContext, SessionContext

    attributes = {index.to_bytes(2, "big") for index in range(1_024)}
    session = SessionContext(None, "session", ApplicationContext("app"))

    with _bind_invocation_context(session) as invocation_context:
        app.publish_attributes(attributes)
        with pytest.raises(ValueError, match="exceeds configured limits"):
            app.publish_attributes({b"overflow"})

    assert invocation_context.attributes == attributes


def test_app_get_resolves_futures():
    """Test the runtime resolves multiple ObjectFutures."""
    from flamepy.app import ObjectFuture
    from flamepy.app.client import _Runtime as Runtime

    app = object.__new__(Runtime)

    f1 = Future()
    f2 = Future()
    f1.set_result(b"ref1")
    f2.set_result(b"ref2")

    with patch("flamepy.app.client.ObjectRef", DummyObjectRef):
        with patch("flamepy.app.client.get_object", side_effect=[{"a": 1}, {"b": 2}]):
            of1 = ObjectFuture(f1)
            of2 = ObjectFuture(f2)

            results = app.get([of1, of2])
            assert results == [{"a": 1}, {"b": 2}]


def test_app_wait_waits_for_all_futures():
    """Test the runtime waits for all futures to complete."""
    from flamepy.app import ObjectFuture
    from flamepy.app.client import _Runtime as Runtime

    app = object.__new__(Runtime)

    f1 = Future()
    f2 = Future()
    f1.set_result(b"done1")
    f2.set_result(b"done2")

    of1 = ObjectFuture(f1)
    of2 = ObjectFuture(f2)

    app.wait([of1, of2])


def test_app_ref_returns_objectrefs():
    """Test the runtime returns ObjectRefs for all futures."""
    from flamepy.app import ObjectFuture
    from flamepy.app.client import _Runtime as Runtime

    app = object.__new__(Runtime)

    f1 = Future()
    f2 = Future()
    f1.set_result(b"ref1")
    f2.set_result(b"ref2")

    with patch("flamepy.app.client.ObjectRef", DummyObjectRef):
        of1 = ObjectFuture(f1)
        of2 = ObjectFuture(f2)

        refs = app.ref([of1, of2])
        assert len(refs) == 2
        assert all(isinstance(r, DummyObjectRef) for r in refs)


# App Type Tests


class TestServiceContext:
    """Tests for ServiceContext dataclass."""

    def test_app_context_default_values(self):
        """Test ServiceContext with default values."""
        ctx = ServiceContext(execution_object=lambda x: x)
        assert ctx.autoscale is True
        assert ctx.warmup == 0
        assert ctx.min_instances == 0
        assert ctx.max_instances is None

    def test_app_context_autoscale_true(self):
        """Test ServiceContext with autoscale=True."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=True)
        assert ctx.min_instances == 0
        assert ctx.max_instances is None

    def test_app_context_autoscale_false(self):
        """Test ServiceContext with autoscale=False."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=False)
        assert ctx.min_instances == 1
        assert ctx.max_instances == 1

    def test_app_context_warmup_with_autoscale(self):
        """Test ServiceContext warmup affects min_instances when autoscale=True."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=True, warmup=5)
        assert ctx.min_instances == 5
        assert ctx.max_instances is None

    def test_app_context_warmup_without_autoscale(self):
        """Test ServiceContext warmup affects both min/max when autoscale=False."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=False, warmup=3)
        assert ctx.min_instances == 3
        assert ctx.max_instances == 3

    def test_app_context_rejects_object_instance(self):
        """Test ServiceContext accepts only function or class definitions."""

        class MyClass:
            pass

        with pytest.raises(TypeError, match="function or class"):
            ServiceContext(execution_object=MyClass())


class TestServiceRequest:
    """Tests for ServiceRequest dataclass."""

    def test_app_request_default_values(self):
        """Test ServiceRequest with default values."""
        req = ServiceRequest()
        assert req.method is None
        assert req.args is None
        assert req.kwargs is None

    def test_app_request_with_method(self):
        """Test ServiceRequest with method name."""
        req = ServiceRequest(method="process")
        assert req.method == "process"

    def test_app_request_with_args(self):
        """Test ServiceRequest with args tuple."""
        req = ServiceRequest(args=(1, 2, 3))
        assert req.args == (1, 2, 3)

    def test_app_request_with_args_list(self):
        """Test ServiceRequest accepts args as list."""
        req = ServiceRequest(args=[1, 2, 3])
        assert req.args == [1, 2, 3]

    def test_app_request_with_kwargs(self):
        """Test ServiceRequest with kwargs dict."""
        req = ServiceRequest(kwargs={"a": 1, "b": 2})
        assert req.kwargs == {"a": 1, "b": 2}

    def test_app_request_complete(self):
        """Test ServiceRequest with all fields."""
        req = ServiceRequest(method="compute", args=(10, 20), kwargs={"scale": 2.0})
        assert req.method == "compute"
        assert req.args == (10, 20)
        assert req.kwargs == {"scale": 2.0}

    def test_app_request_invalid_method_type(self):
        """Test ServiceRequest rejects non-string method."""
        with pytest.raises(ValueError, match="method must be a string or None"):
            ServiceRequest(method=123)

    def test_app_request_invalid_args_type(self):
        """Test ServiceRequest rejects non-tuple/list args."""
        with pytest.raises(ValueError, match="args must be a tuple or list"):
            ServiceRequest(args="not a tuple")

    def test_app_request_invalid_kwargs_type(self):
        """Test ServiceRequest rejects non-dict kwargs."""
        with pytest.raises(ValueError, match="kwargs must be a dict"):
            ServiceRequest(kwargs="not a dict")

    def test_app_request_empty_args_tuple(self):
        """Test ServiceRequest with empty args tuple."""
        req = ServiceRequest(args=())
        assert req.args == ()

    def test_app_request_empty_kwargs_dict(self):
        """Test ServiceRequest with empty kwargs dict."""
        req = ServiceRequest(kwargs={})
        assert req.kwargs == {}

    def test_app_request_complex_args(self):
        """Test ServiceRequest with complex nested args."""
        complex_args = (
            {"nested": [1, 2, 3]},
            [4, 5, 6],
            None,
            "string",
        )
        req = ServiceRequest(args=complex_args)
        assert req.args == complex_args

    def test_app_request_complex_kwargs(self):
        """Test ServiceRequest with complex nested kwargs."""
        complex_kwargs = {
            "data": {"key": "value"},
            "items": [1, 2, 3],
            "flag": True,
            "nothing": None,
        }
        req = ServiceRequest(kwargs=complex_kwargs)
        assert req.kwargs == complex_kwargs


class TestServiceContextEdgeCases:
    """Additional edge case tests for ServiceContext."""

    def test_app_context_with_lambda(self):
        """Test ServiceContext with lambda as execution_object."""
        ctx = ServiceContext(execution_object=lambda x: x * 2)
        assert callable(ctx.execution_object)

    def test_app_context_with_builtin_function(self):
        """Test ServiceContext with builtin function as execution_object."""
        ctx = ServiceContext(execution_object=len)
        assert ctx.execution_object is len

    def test_app_context_warmup_zero(self):
        """Test ServiceContext with warmup=0 and autoscale=True."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=True, warmup=0)
        assert ctx.min_instances == 0
        assert ctx.max_instances is None

    def test_app_context_warmup_zero_no_autoscale(self):
        """Test ServiceContext with warmup=0 and autoscale=False."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=False, warmup=0)
        assert ctx.min_instances == 1
        assert ctx.max_instances == 1

    def test_app_context_large_warmup(self):
        """Test ServiceContext with large warmup value."""
        ctx = ServiceContext(execution_object=lambda x: x, autoscale=True, warmup=1000)
        assert ctx.min_instances == 1000
        assert ctx.max_instances is None

    def test_app_context_rejects_callable_instance(self):
        """Callable instances are objects, not function service definitions."""

        class StatefulService:
            def __call__(self):
                return None

        with pytest.raises(TypeError, match="function or class"):
            ServiceContext(execution_object=StatefulService())


class TestServiceRequestEdgeCases:
    """Additional edge case tests for ServiceRequest."""

    def test_app_request_none_method_explicit(self):
        """Test ServiceRequest with method explicitly set to None."""
        req = ServiceRequest(method=None, args=(1, 2), kwargs={"a": 1})
        assert req.method is None
        assert req.args == (1, 2)
        assert req.kwargs == {"a": 1}

    def test_app_request_nested_objectref_in_args(self):
        """Test ServiceRequest with complex nested structures in args."""
        nested_args = ({"nested": {"deep": [1, 2, 3]}}, [{"a": 1}, {"b": 2}])
        req = ServiceRequest(args=nested_args)
        assert req.args == nested_args

    def test_app_request_large_args(self):
        """Test ServiceRequest with large number of args."""
        large_args = tuple(range(1000))
        req = ServiceRequest(args=large_args)
        assert len(req.args) == 1000

    def test_app_request_callable_in_kwargs(self):
        """Test ServiceRequest with callable in kwargs."""
        req = ServiceRequest(kwargs={"callback": lambda x: x})
        assert callable(req.kwargs["callback"])

    def test_app_request_bytes_in_args(self):
        """Test ServiceRequest with bytes in args."""
        req = ServiceRequest(args=(b"binary data", b"\x00\x01\x02"))
        assert req.args[0] == b"binary data"
        assert req.args[1] == b"\x00\x01\x02"


def test_service_instance_close_retries_after_session_close_failure():
    """A transient session-close failure must not make cleanup unreachable."""
    from flamepy.app import ServiceInstance

    close = MagicMock(side_effect=[RuntimeError("transient close failure"), None])
    owner = MagicMock()
    owner.close = close
    service = object.__new__(ServiceInstance)
    service._app = "retry-close-app"
    service._future_lock = threading.Lock()
    service._state_changed = threading.Condition(service._future_lock)
    service._pending_futures = set()
    service._state = _ServiceState.OPEN
    service._session_owner = owner

    with pytest.raises(RuntimeError, match="transient close failure"):
        service.close()

    assert service._state is _ServiceState.CLOSE_FAILED
    service.close()
    service.close()

    assert close.call_count == 2
    assert service._state is _ServiceState.CLOSED


def test_destroy_retains_runtime_when_close_fails_for_retry(monkeypatch):
    """A failed destroy leaves the process runtime reachable for another try."""
    import flamepy.app as app

    runtime = MagicMock()
    runtime.close.side_effect = [RuntimeError("transient close failure"), None]
    monkeypatch.setattr(app_client, "_runtime", runtime)

    with pytest.raises(RuntimeError, match="transient close failure"):
        app.destroy()

    assert app_client._runtime is runtime

    app.destroy()

    assert app_client._runtime is None
    assert runtime.close.call_count == 2


def test_unregister_failure_preserves_cache_and_package_artifacts(monkeypatch):
    """Artifacts remain available while the application is still registered."""
    runtime = object.__new__(Runtime)
    runtime._name = "unregister-failure-app"
    runtime._cleanup_package_artifacts = MagicMock()
    delete_objects = MagicMock()
    monkeypatch.setattr(
        "flamepy.app.client.core_client.unregister_application",
        MagicMock(side_effect=RuntimeError("control plane unavailable")),
    )
    monkeypatch.setattr("flamepy.core.cache.delete_objects", delete_objects)

    with pytest.raises(RuntimeError, match="control plane unavailable"):
        runtime._unregister_application()

    delete_objects.assert_not_called()
    runtime._cleanup_package_artifacts.assert_not_called()


def test_unregister_leaves_registered_package_cleanup_to_object_cache(monkeypatch):
    """Successful teardown does not delete the remotely registered package."""
    runtime = object.__new__(Runtime)
    runtime._name = "draining-app"
    runtime._cleanup_local_package = MagicMock()
    runtime._cleanup_storage = MagicMock()
    unregister = MagicMock()
    delete_objects = MagicMock()
    monkeypatch.setattr("flamepy.app.client.core_client.unregister_application", unregister)
    monkeypatch.setattr("flamepy.core.cache.delete_objects", delete_objects)

    runtime._unregister_application()

    unregister.assert_called_once_with("draining-app")
    runtime._cleanup_local_package.assert_called_once_with()
    runtime._cleanup_storage.assert_not_called()
    delete_objects.assert_not_called()


def test_runtime_close_serializes_with_service_creation(monkeypatch):
    """Closing concurrently with service creation cannot orphan its session."""
    import threading

    constructor_entered = threading.Event()
    allow_constructor = threading.Event()
    close_finished = threading.Event()
    created = []
    creation_errors = []
    close_errors = []

    class BlockingServiceInstance:
        def __init__(self, *args, **kwargs):
            self.close = MagicMock()
            created.append(self)
            constructor_entered.set()
            assert allow_constructor.wait(timeout=5)

    monkeypatch.setattr(Runtime, "_start", lambda self: None)
    runtime = Runtime("concurrent-lifecycle-app")
    runtime._state = _RuntimeState.ACTIVE
    runtime._application_owner = MagicMock()
    monkeypatch.setattr(app_client, "ServiceInstance", BlockingServiceInstance)

    def execution_object():
        return None

    def create_service():
        try:
            runtime.service()(execution_object)
        except Exception as error:
            creation_errors.append(error)

    def close_runtime():
        try:
            runtime.close()
        except Exception as error:
            close_errors.append(error)
        finally:
            close_finished.set()

    create_thread = threading.Thread(target=create_service)
    close_thread = threading.Thread(target=close_runtime)
    create_thread.start()
    assert constructor_entered.wait(timeout=5)
    close_thread.start()

    # close() must not finish while a service is between construction and
    # registration in runtime._services.
    assert not close_finished.wait(timeout=0.1)
    allow_constructor.set()
    create_thread.join(timeout=5)
    close_thread.join(timeout=5)

    assert not create_thread.is_alive()
    assert not close_thread.is_alive()
    assert creation_errors == []
    assert close_errors == []
    assert len(created) == 1
    created[0].close.assert_called_once_with()
    runtime._application_owner.unregister.assert_called_once_with()
    assert runtime._services == []
    assert runtime._state is _RuntimeState.INACTIVE
