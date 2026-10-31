"""Flame never guesses where the cluster is: an endpoint that is configured nowhere is an error."""

import pytest

from flamepy.core.types import FlameContext, FlameError, FlameErrorCode


@pytest.fixture
def home(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    for name in ("FLAME_ENDPOINT", "FLAME_CACHE_ENDPOINT", "FLAME_CACHE_STORAGE", "FLAME_CA_FILE", "FLAME_CERT_FILE", "FLAME_KEY_FILE"):
        monkeypatch.delenv(name, raising=False)
    return tmp_path


def write_config(home, body):
    (home / ".flame").mkdir()
    (home / ".flame/flame.yaml").write_text(body)


def test_no_file_and_no_environment_is_an_error_that_says_how_to_configure(home):
    context = FlameContext()

    with pytest.raises(FlameError) as error:
        context.endpoint

    assert error.value.code == FlameErrorCode.INVALID_CONFIG
    message = str(error.value)
    assert str(home / ".flame/flame.yaml") in message
    assert "FLAME_ENDPOINT" in message
    assert 'endpoint: "http://<head>:8080"' in message


def test_the_environment_alone_names_the_cluster(home, monkeypatch):
    monkeypatch.setenv("FLAME_ENDPOINT", "http://head:8080")
    monkeypatch.setenv("FLAME_CACHE_ENDPOINT", "grpc://worker:9090")

    context = FlameContext()

    assert context.endpoint == "http://head:8080"
    assert context.cache_endpoint == "grpc://worker:9090"


def test_a_file_without_a_cluster_endpoint_is_an_error(home):
    write_config(home, "current-context: flame\ncontexts:\n  - name: flame\n    cluster: {}\n")

    with pytest.raises(FlameError, match="cluster.endpoint is not set"):
        FlameContext().endpoint


def test_a_file_without_a_cache_has_no_cache_and_no_default_one(home):
    write_config(
        home,
        'current-context: flame\ncontexts:\n  - name: flame\n    cluster:\n      endpoint: "http://head:8080"\n',
    )

    context = FlameContext()

    assert context.endpoint == "http://head:8080"
    assert context.cache is None
    with pytest.raises(FlameError, match="cache.endpoint is not set"):
        context.cache_endpoint


def test_the_default_endpoints_are_gone():
    import flamepy

    assert not hasattr(flamepy, "DEFAULT_FLAME_ENDPOINT")
    assert not hasattr(flamepy, "DEFAULT_FLAME_CACHE_ENDPOINT")
