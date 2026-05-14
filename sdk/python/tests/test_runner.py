"""Tests for flamepy runner APIs."""

from concurrent.futures import Future
from unittest.mock import MagicMock, patch

import pytest

from flamepy.runner.storage import CacheStorage, FileStorage, create_storage_backend
from flamepy.runner.types import RunnerContext, RunnerRequest, SessionContext

# Runner Storage Tests


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


# Runner Service Tests


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
    from flamepy.runner.runner import ObjectFuture

    future = Future()
    future.set_result(b"encoded-ref")

    with patch("flamepy.runner.runner.ObjectRef", DummyObjectRef):
        of = ObjectFuture(future)
        ref = of.ref()
        assert isinstance(ref, DummyObjectRef)
        assert ref._data == b"encoded-ref"


def test_objectfuture_ref_returns_existing_objectref():
    """Test ObjectFuture.ref() returns ObjectRef if already decoded."""
    from flamepy.runner.runner import ObjectFuture

    dummy_ref = DummyObjectRef(b"already-ref")
    future = Future()
    future.set_result(dummy_ref)

    with patch("flamepy.runner.runner.ObjectRef", DummyObjectRef):
        of = ObjectFuture(future)
        ref = of.ref()
        assert ref is dummy_ref


def test_objectfuture_get_retrieves_object():
    """Test ObjectFuture.get() retrieves actual object from cache."""
    from flamepy.runner.runner import ObjectFuture

    future = Future()
    future.set_result(b"encoded-ref")

    with patch("flamepy.runner.runner.ObjectRef", DummyObjectRef):
        with patch("flamepy.runner.runner.get_object", return_value={"key": "value"}):
            of = ObjectFuture(future)
            result = of.get()
            assert result == {"key": "value"}


def test_objectfuture_wait_blocks_until_done():
    """Test ObjectFuture.wait() blocks until future completes."""
    from flamepy.runner.runner import ObjectFuture

    future = Future()
    future.set_result(b"done")

    of = ObjectFuture(future)
    of.wait()


def test_objectfuture_iterator_yields_in_completion_order():
    """Test ObjectFutureIterator yields futures as they complete."""
    from flamepy.runner.runner import ObjectFuture, ObjectFutureIterator

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


def test_runner_should_exclude_matches_patterns():
    """Test Runner._should_exclude() matches exclusion patterns."""
    from flamepy.runner.runner import Runner

    runner = object.__new__(Runner)

    assert runner._should_exclude("__pycache__", ["__pycache__"])
    assert runner._should_exclude("test.pyc", ["*.pyc"])
    assert runner._should_exclude(".git", [".git", ".venv"])
    assert not runner._should_exclude("main.py", ["*.pyc", "__pycache__"])


def test_runner_should_exclude_handles_nested_paths():
    """Test Runner._should_exclude() handles nested path patterns."""
    from flamepy.runner.runner import Runner

    runner = object.__new__(Runner)

    assert runner._should_exclude("src/__pycache__/module.pyc", ["*.pyc"])
    assert runner._should_exclude("tests/data/file.tmp", ["*.tmp"])


def test_runnerservice_generates_method_wrappers():
    """Test RunnerService generates wrappers for public methods."""
    from flamepy.runner.runner import RunnerService

    class Calculator:
        def add(self, a, b):
            return a + b

        def multiply(self, a, b):
            return a * b

        def _private(self):
            pass

    calc = Calculator()
    rs = object.__new__(RunnerService)
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


def test_runnerservice_callable_for_function():
    """Test RunnerService is callable when execution object is a function."""
    from flamepy.runner.runner import RunnerService

    def my_func(x):
        return x * 2

    rs = object.__new__(RunnerService)
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


def test_runnerservice_not_callable_for_class():
    """Test RunnerService raises TypeError when called but object is class."""
    from flamepy.runner.runner import RunnerService

    class MyClass:
        def method(self):
            pass

    rs = object.__new__(RunnerService)
    rs._app = "test-app"
    rs._execution_object = MyClass()
    rs._function_wrapper = None

    mock_session = MagicMock()
    rs._session = mock_session

    rs._generate_wrappers()

    with pytest.raises(TypeError):
        rs()


def test_runnerservice_close_closes_session():
    """Test RunnerService.close() closes the underlying session."""
    from flamepy.runner.runner import RunnerService

    rs = object.__new__(RunnerService)
    rs._app = "test-app"

    mock_session = MagicMock()
    rs._session = mock_session

    rs.close()

    mock_session.close.assert_called_once()


def test_runnerservice_does_not_overwrite_close_with_user_method():
    """Test generated wrappers do not replace RunnerService lifecycle methods."""
    from flamepy.runner.runner import RunnerService

    class ServiceWithClose:
        def close(self):
            return "user-close"

    rs = object.__new__(RunnerService)
    rs._app = "test-app"
    rs._execution_object = ServiceWithClose()
    rs._function_wrapper = None
    rs._session = MagicMock()

    rs._generate_wrappers()
    rs.close()

    rs._session.close.assert_called_once()


def test_runpy_resolves_object_ref_to_cached_none():
    """Test cached None is a valid ObjectRef value, not a retrieval miss."""
    from flamepy.core.cache import ObjectRef
    from flamepy.runner.runpy import FlameRunpyService

    svc = FlameRunpyService()
    ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/object", version=1)

    with patch("flamepy.runner.runpy.get_object", return_value=None):
        assert svc._resolve_object_ref(ref) is None
        args, kwargs = svc._resolve_object_refs_parallel((ref,), {"value": ref})
        assert args == (None,)
        assert kwargs == {"value": None}


def test_runner_get_resolves_futures():
    """Test Runner.get() resolves multiple ObjectFutures."""
    from flamepy.runner.runner import ObjectFuture, Runner

    runner = object.__new__(Runner)

    f1 = Future()
    f2 = Future()
    f1.set_result(b"ref1")
    f2.set_result(b"ref2")

    with patch("flamepy.runner.runner.ObjectRef", DummyObjectRef):
        with patch("flamepy.runner.runner.get_object", side_effect=[{"a": 1}, {"b": 2}]):
            of1 = ObjectFuture(f1)
            of2 = ObjectFuture(f2)

            results = runner.get([of1, of2])
            assert results == [{"a": 1}, {"b": 2}]


def test_runner_wait_waits_for_all_futures():
    """Test Runner.wait() waits for all futures to complete."""
    from flamepy.runner.runner import ObjectFuture, Runner

    runner = object.__new__(Runner)

    f1 = Future()
    f2 = Future()
    f1.set_result(b"done1")
    f2.set_result(b"done2")

    of1 = ObjectFuture(f1)
    of2 = ObjectFuture(f2)

    runner.wait([of1, of2])


def test_runner_ref_returns_objectrefs():
    """Test Runner.ref() returns ObjectRefs for all futures."""
    from flamepy.runner.runner import ObjectFuture, Runner

    runner = object.__new__(Runner)

    f1 = Future()
    f2 = Future()
    f1.set_result(b"ref1")
    f2.set_result(b"ref2")

    with patch("flamepy.runner.runner.ObjectRef", DummyObjectRef):
        of1 = ObjectFuture(f1)
        of2 = ObjectFuture(f2)

        refs = runner.ref([of1, of2])
        assert len(refs) == 2
        assert all(isinstance(r, DummyObjectRef) for r in refs)


# Runner Type Tests


class TestSessionContext:
    """Tests for SessionContext dataclass validation."""

    def test_session_context_default_values(self):
        """Test SessionContext with default values."""
        ctx = SessionContext()
        assert ctx.session_id is None
        assert ctx.application_name is None

    def test_session_context_with_session_id(self):
        """Test SessionContext with valid session_id."""
        ctx = SessionContext(session_id="my-session-001")
        assert ctx.session_id == "my-session-001"
        assert ctx.application_name is None

    def test_session_context_with_application_name(self):
        """Test SessionContext with application_name."""
        ctx = SessionContext(application_name="my-app")
        assert ctx.session_id is None
        assert ctx.application_name == "my-app"

    def test_session_context_with_both_fields(self):
        """Test SessionContext with both fields set."""
        ctx = SessionContext(session_id="sess-001", application_name="app-001")
        assert ctx.session_id == "sess-001"
        assert ctx.application_name == "app-001"

    def test_session_context_invalid_session_id_type(self):
        """Test SessionContext rejects non-string session_id."""
        with pytest.raises(ValueError, match="session_id must be a string"):
            SessionContext(session_id=12345)

    def test_session_context_empty_session_id(self):
        """Test SessionContext rejects empty session_id."""
        with pytest.raises(ValueError, match="session_id cannot be empty"):
            SessionContext(session_id="")

    def test_session_context_session_id_too_long(self):
        """Test SessionContext rejects session_id > 128 chars."""
        long_id = "x" * 129
        with pytest.raises(ValueError, match="session_id too long"):
            SessionContext(session_id=long_id)

    def test_session_context_max_length_session_id(self):
        """Test SessionContext accepts session_id of exactly 128 chars."""
        max_id = "x" * 128
        ctx = SessionContext(session_id=max_id)
        assert len(ctx.session_id) == 128

    def test_session_context_invalid_application_name_type(self):
        """Test SessionContext rejects non-string application_name."""
        with pytest.raises(ValueError, match="application_name must be a string"):
            SessionContext(application_name=42)


class TestRunnerContext:
    """Tests for RunnerContext dataclass."""

    def test_runner_context_default_values(self):
        """Test RunnerContext with default values."""
        ctx = RunnerContext(execution_object=lambda x: x)
        assert ctx.stateful is False
        assert ctx.autoscale is True
        assert ctx.warmup == 0
        assert ctx.min_instances == 0
        assert ctx.max_instances is None

    def test_runner_context_autoscale_true(self):
        """Test RunnerContext with autoscale=True."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=True)
        assert ctx.min_instances == 0
        assert ctx.max_instances is None

    def test_runner_context_autoscale_false(self):
        """Test RunnerContext with autoscale=False."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=False)
        assert ctx.min_instances == 1
        assert ctx.max_instances == 1

    def test_runner_context_warmup_with_autoscale(self):
        """Test RunnerContext warmup affects min_instances when autoscale=True."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=True, warmup=5)
        assert ctx.min_instances == 5
        assert ctx.max_instances is None

    def test_runner_context_warmup_without_autoscale(self):
        """Test RunnerContext warmup affects both min/max when autoscale=False."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=False, warmup=3)
        assert ctx.min_instances == 3
        assert ctx.max_instances == 3

    def test_runner_context_stateful_with_instance(self):
        """Test RunnerContext stateful=True is allowed for instances."""

        class MyClass:
            pass

        instance = MyClass()
        ctx = RunnerContext(execution_object=instance, stateful=True)
        assert ctx.stateful is True

    def test_runner_context_stateful_with_function(self):
        """Test RunnerContext stateful=True is allowed for functions."""

        def my_func():
            pass

        ctx = RunnerContext(execution_object=my_func, stateful=True)
        assert ctx.stateful is True

    def test_runner_context_stateful_with_class_raises(self):
        """Test RunnerContext stateful=True raises for classes."""

        class MyClass:
            pass

        with pytest.raises(ValueError, match="Cannot set stateful=True for a class"):
            RunnerContext(execution_object=MyClass, stateful=True)

    def test_runner_context_stateful_false_with_class(self):
        """Test RunnerContext stateful=False is allowed for classes."""

        class MyClass:
            pass

        ctx = RunnerContext(execution_object=MyClass, stateful=False)
        assert ctx.stateful is False


class TestRunnerRequest:
    """Tests for RunnerRequest dataclass."""

    def test_runner_request_default_values(self):
        """Test RunnerRequest with default values."""
        req = RunnerRequest()
        assert req.method is None
        assert req.args is None
        assert req.kwargs is None

    def test_runner_request_with_method(self):
        """Test RunnerRequest with method name."""
        req = RunnerRequest(method="process")
        assert req.method == "process"

    def test_runner_request_with_args(self):
        """Test RunnerRequest with args tuple."""
        req = RunnerRequest(args=(1, 2, 3))
        assert req.args == (1, 2, 3)

    def test_runner_request_with_args_list(self):
        """Test RunnerRequest accepts args as list."""
        req = RunnerRequest(args=[1, 2, 3])
        assert req.args == [1, 2, 3]

    def test_runner_request_with_kwargs(self):
        """Test RunnerRequest with kwargs dict."""
        req = RunnerRequest(kwargs={"a": 1, "b": 2})
        assert req.kwargs == {"a": 1, "b": 2}

    def test_runner_request_complete(self):
        """Test RunnerRequest with all fields."""
        req = RunnerRequest(method="compute", args=(10, 20), kwargs={"scale": 2.0})
        assert req.method == "compute"
        assert req.args == (10, 20)
        assert req.kwargs == {"scale": 2.0}

    def test_runner_request_invalid_method_type(self):
        """Test RunnerRequest rejects non-string method."""
        with pytest.raises(ValueError, match="method must be a string or None"):
            RunnerRequest(method=123)

    def test_runner_request_invalid_args_type(self):
        """Test RunnerRequest rejects non-tuple/list args."""
        with pytest.raises(ValueError, match="args must be a tuple or list"):
            RunnerRequest(args="not a tuple")

    def test_runner_request_invalid_kwargs_type(self):
        """Test RunnerRequest rejects non-dict kwargs."""
        with pytest.raises(ValueError, match="kwargs must be a dict"):
            RunnerRequest(kwargs="not a dict")

    def test_runner_request_empty_args_tuple(self):
        """Test RunnerRequest with empty args tuple."""
        req = RunnerRequest(args=())
        assert req.args == ()

    def test_runner_request_empty_kwargs_dict(self):
        """Test RunnerRequest with empty kwargs dict."""
        req = RunnerRequest(kwargs={})
        assert req.kwargs == {}

    def test_runner_request_complex_args(self):
        """Test RunnerRequest with complex nested args."""
        complex_args = (
            {"nested": [1, 2, 3]},
            [4, 5, 6],
            None,
            "string",
        )
        req = RunnerRequest(args=complex_args)
        assert req.args == complex_args

    def test_runner_request_complex_kwargs(self):
        """Test RunnerRequest with complex nested kwargs."""
        complex_kwargs = {
            "data": {"key": "value"},
            "items": [1, 2, 3],
            "flag": True,
            "nothing": None,
        }
        req = RunnerRequest(kwargs=complex_kwargs)
        assert req.kwargs == complex_kwargs


class TestSessionContextEdgeCases:
    """Additional edge case tests for SessionContext."""

    def test_session_context_whitespace_only_session_id(self):
        """Test SessionContext accepts whitespace-only session_id (not empty)."""
        ctx = SessionContext(session_id="   ")
        assert ctx.session_id == "   "

    def test_session_context_special_characters_in_session_id(self):
        """Test SessionContext with special characters in session_id."""
        ctx = SessionContext(session_id="sess-123_abc.test")
        assert ctx.session_id == "sess-123_abc.test"

    def test_session_context_unicode_session_id(self):
        """Test SessionContext with unicode characters in session_id."""
        ctx = SessionContext(session_id="session-日本語-test")
        assert ctx.session_id == "session-日本語-test"

    def test_session_context_boundary_length_session_id(self):
        """Test SessionContext with session_id at boundary lengths."""
        ctx_1 = SessionContext(session_id="x")
        assert len(ctx_1.session_id) == 1

        ctx_127 = SessionContext(session_id="x" * 127)
        assert len(ctx_127.session_id) == 127

        ctx_128 = SessionContext(session_id="x" * 128)
        assert len(ctx_128.session_id) == 128

    def test_session_context_empty_application_name(self):
        """Test SessionContext with empty application_name (allowed)."""
        ctx = SessionContext(application_name="")
        assert ctx.application_name == ""


class TestRunnerContextEdgeCases:
    """Additional edge case tests for RunnerContext."""

    def test_runner_context_with_lambda(self):
        """Test RunnerContext with lambda as execution_object."""
        ctx = RunnerContext(execution_object=lambda x: x * 2)
        assert callable(ctx.execution_object)

    def test_runner_context_with_builtin_function(self):
        """Test RunnerContext with builtin function as execution_object."""
        ctx = RunnerContext(execution_object=len)
        assert ctx.execution_object is len

    def test_runner_context_warmup_zero(self):
        """Test RunnerContext with warmup=0 and autoscale=True."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=True, warmup=0)
        assert ctx.min_instances == 0
        assert ctx.max_instances is None

    def test_runner_context_warmup_zero_no_autoscale(self):
        """Test RunnerContext with warmup=0 and autoscale=False."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=False, warmup=0)
        assert ctx.min_instances == 1
        assert ctx.max_instances == 1

    def test_runner_context_large_warmup(self):
        """Test RunnerContext with large warmup value."""
        ctx = RunnerContext(execution_object=lambda x: x, autoscale=True, warmup=1000)
        assert ctx.min_instances == 1000
        assert ctx.max_instances is None

    def test_runner_context_instance_with_state(self):
        """Test RunnerContext with stateful instance."""

        class StatefulService:
            def __init__(self):
                self.counter = 0

            def increment(self):
                self.counter += 1
                return self.counter

        instance = StatefulService()
        ctx = RunnerContext(execution_object=instance, stateful=True)
        assert ctx.stateful is True
        assert ctx.execution_object is instance


class TestRunnerRequestEdgeCases:
    """Additional edge case tests for RunnerRequest."""

    def test_runner_request_none_method_explicit(self):
        """Test RunnerRequest with method explicitly set to None."""
        req = RunnerRequest(method=None, args=(1, 2), kwargs={"a": 1})
        assert req.method is None
        assert req.args == (1, 2)
        assert req.kwargs == {"a": 1}

    def test_runner_request_nested_objectref_in_args(self):
        """Test RunnerRequest with complex nested structures in args."""
        nested_args = ({"nested": {"deep": [1, 2, 3]}}, [{"a": 1}, {"b": 2}])
        req = RunnerRequest(args=nested_args)
        assert req.args == nested_args

    def test_runner_request_large_args(self):
        """Test RunnerRequest with large number of args."""
        large_args = tuple(range(1000))
        req = RunnerRequest(args=large_args)
        assert len(req.args) == 1000

    def test_runner_request_callable_in_kwargs(self):
        """Test RunnerRequest with callable in kwargs."""
        req = RunnerRequest(kwargs={"callback": lambda x: x})
        assert callable(req.kwargs["callback"])

    def test_runner_request_bytes_in_args(self):
        """Test RunnerRequest with bytes in args."""
        req = RunnerRequest(args=(b"binary data", b"\x00\x01\x02"))
        assert req.args[0] == b"binary data"
        assert req.args[1] == b"\x00\x01\x02"
