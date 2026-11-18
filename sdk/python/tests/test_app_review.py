"""Regression coverage for App deployment compatibility and artifact ownership."""

import sys
import tomllib
from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

import flamepy.app.client as client
from flamepy.app.storage import CacheStorage, FileStorage, HttpStorage
from flamepy.core.types import ApplicationState, FlameError, FlameErrorCode, Shim


@pytest.fixture
def deployment(tmp_path, monkeypatch):
    store = tmp_path / "store"
    store.mkdir()
    archive = tmp_path / "package.tar.gz"
    archive.write_bytes(b"application archive")
    version = f"{sys.version_info.major}.{sys.version_info.minor}"
    template = SimpleNamespace(shim=Shim.HOST, image=None, command="flmrun-service", working_directory=None, environments=None, labels=None, arguments=None, max_instances=None, delay_release=None, schema=None, installer=None)
    context = SimpleNamespace(package=SimpleNamespace(storage=store.as_uri()), cache=None, app="flmrun")
    registrations = {}

    def lookup(name):
        return template if name == "flmrun" else registrations.get(name)

    def register(name, attributes):
        registrations[name] = SimpleNamespace(url=attributes.url, environments=attributes.environments, state=ApplicationState.ENABLED)

    register_rpc = MagicMock(side_effect=register)
    unregister_rpc = MagicMock(side_effect=lambda name: registrations.pop(name, None))
    monkeypatch.setattr(client, "FlameContext", lambda: context)
    monkeypatch.setattr(client._Runtime, "_create_package", lambda self: str(archive))
    monkeypatch.setattr(client.core_client, "get_application", lookup)
    monkeypatch.setattr(client.core_client, "register_application", register_rpc)
    monkeypatch.setattr(client.core_client, "unregister_application", unregister_rpc)
    return SimpleNamespace(store=store, archive=archive, version=version, template=template, registrations=registrations, register=register, register_rpc=register_rpc, unregister_rpc=unregister_rpc, lookup=lookup)


def test_dependency_markers_and_names_are_valid_toml():
    runtime = object.__new__(client._Runtime)
    runtime._name = 'name"\\test'
    runtime._dependencies = ['numpy; python_version >= "3.12"', "pkg @ file:///C:/path\\name"]
    metadata = tomllib.loads(runtime._generated_pyproject_toml(set()))
    assert metadata["project"]["name"] == runtime._name
    assert metadata["project"]["dependencies"] == sorted(runtime._dependencies)


@pytest.mark.parametrize("scheme", ["grpc", "grpcs", "grpc-proxy", "grpcs-proxy"])
def test_package_backend_preserves_ipv6_endpoint(monkeypatch, tmp_path, scheme):
    archive = tmp_path / "app.tar.gz"
    archive.write_bytes(b"archive")
    endpoint = f"{scheme}://[::1]:9090"
    from flamepy.core import cache

    upload = MagicMock(return_value=SimpleNamespace(endpoint="grpc://[::1]:9090", key="app/pkg/archive"))
    monkeypatch.setattr(cache, "upload_package", upload)
    backend = CacheStorage(endpoint, app_name="app")
    backend.upload(str(archive), archive.name)
    assert upload.call_args.kwargs["endpoint"] == endpoint


@pytest.mark.parametrize("version", ["3.10", "3.99", "invalid"])
def test_explicit_incompatible_python_rejected_before_packaging(deployment, version):
    with pytest.raises(FlameError, match="same Python minor"):
        client._Runtime("app", python_version=version)
    deployment.register_rpc.assert_not_called()
    assert list(deployment.store.iterdir()) == []


@pytest.mark.parametrize("version", [None, "3.99"])
def test_borrowed_application_requires_verified_compatible_python(deployment, version):
    deployment.registrations["app"] = SimpleNamespace(url="file:///borrowed", state=ApplicationState.ENABLED, environments={} if version is None else {"FLAME_PYTHON_VERSION": version})
    with pytest.raises(FlameError, match="Python"):
        client._Runtime("app", python_version=deployment.version)
    deployment.register_rpc.assert_not_called()
    deployment.unregister_rpc.assert_not_called()


def test_registration_response_loss_adopts_committed_package(deployment):
    def commit_then_timeout(name, attributes):
        deployment.register(name, attributes)
        raise TimeoutError("registration response lost")

    deployment.register_rpc.side_effect = commit_then_timeout
    runtime = client._Runtime("app")
    assert runtime._state is client._RuntimeState.ACTIVE
    assert (deployment.store / deployment.archive.name).exists()
    runtime.close()
    assert not (deployment.store / deployment.archive.name).exists()


def test_uncertain_registration_preserves_remote_package(deployment, monkeypatch):
    deployment.register_rpc.side_effect = TimeoutError("registration response lost")
    queries = 0

    def lookup(name):
        nonlocal queries
        if name != "flmrun":
            queries += 1
            if queries > 1:
                raise TimeoutError("lookup unavailable")
        return deployment.lookup(name)

    monkeypatch.setattr(client.core_client, "get_application", lookup)
    with pytest.raises(FlameError, match="registration response lost"):
        client._Runtime("app")
    assert (deployment.store / deployment.archive.name).exists()
    deployment.unregister_rpc.assert_not_called()


def test_definitive_registration_failure_reclaims_only_attempt(deployment):
    deployment.register_rpc.side_effect = FlameError(FlameErrorCode.INVALID_ARGUMENT, "rejected before commit")
    with pytest.raises(FlameError, match="rejected before commit"):
        client._Runtime("app")
    assert list(deployment.store.iterdir()) == []


def test_external_package_waits_for_authoritative_retirement(deployment, monkeypatch):
    runtime = client._Runtime("app")
    package = deployment.store / deployment.archive.name
    polls = []
    deployment.unregister_rpc.side_effect = lambda name: setattr(deployment.registrations[name], "state", ApplicationState.DISABLED)

    def wait(_):
        assert package.exists()
        polls.append(True)
        deployment.registrations.clear()

    monkeypatch.setattr(client.time, "sleep", wait)
    runtime.close()
    assert polls == [True]
    assert not package.exists()


def test_retirement_timeout_is_retryable_without_touching_replacement(deployment, monkeypatch):
    runtime = client._Runtime("app")
    package = deployment.store / deployment.archive.name
    deployment.unregister_rpc.side_effect = lambda name: setattr(deployment.registrations[name], "state", ApplicationState.DISABLED)
    monkeypatch.setattr(client, "_APPLICATION_RETIREMENT_TIMEOUT", 0)
    with pytest.raises(FlameError, match="still retiring"):
        runtime.close()
    assert runtime._state is client._RuntimeState.CLOSE_FAILED
    assert package.exists()
    replacement = SimpleNamespace(url="file:///different-package", state=ApplicationState.ENABLED)
    deployment.registrations["app"] = replacement
    runtime.close()
    deployment.unregister_rpc.assert_called_once_with("app")
    assert deployment.registrations["app"] is replacement
    assert not package.exists()


def test_retirement_retry_skips_already_disabled_registration(deployment, monkeypatch):
    runtime = client._Runtime("app")
    deployment.unregister_rpc.side_effect = lambda name: setattr(deployment.registrations[name], "state", ApplicationState.DISABLED)
    monkeypatch.setattr(client, "_APPLICATION_RETIREMENT_TIMEOUT", 0)
    with pytest.raises(FlameError, match="still retiring"):
        runtime.close()

    def finish_retiring(name):
        current = deployment.registrations.pop(name, None)
        return current

    monkeypatch.setattr(client.core_client, "get_application", finish_retiring)
    runtime.close()
    deployment.unregister_rpc.assert_called_once_with("app")


def test_external_cleanup_failure_keeps_close_retryable(deployment, monkeypatch):
    runtime = client._Runtime("app")
    original_delete = FileStorage.delete
    delete = MagicMock(side_effect=OSError("read-only storage"))
    monkeypatch.setattr(FileStorage, "delete", delete)
    with pytest.raises(OSError, match="read-only storage"):
        runtime.close()
    assert runtime._state is client._RuntimeState.CLOSE_FAILED
    monkeypatch.setattr(FileStorage, "delete", original_delete)
    runtime.close()
    assert not (deployment.store / deployment.archive.name).exists()
    deployment.unregister_rpc.assert_called_once_with("app")


def test_http_delete_failure_propagates_for_retry(monkeypatch):
    from flamepy.app import storage

    monkeypatch.setattr(storage.requests, "delete", lambda *args, **kwargs: SimpleNamespace(status_code=503))
    with pytest.raises(FlameError, match="HTTP 503"):
        HttpStorage("https://storage.example/pkg").delete("app.tar.gz")


@pytest.mark.parametrize("failure", [TimeoutError("pending commit"), FlameError(FlameErrorCode.INTERNAL, "pending commit")])
def test_negative_lookup_does_not_delete_uncertain_registration_package(deployment, failure):
    deployment.register_rpc.side_effect = failure
    with pytest.raises(FlameError, match="pending commit"):
        client._Runtime("app")
    assert deployment.lookup("app") is None
    assert (deployment.store / deployment.archive.name).exists()
