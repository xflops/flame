import time
from datetime import datetime, timezone

import flamepy.core.client as client


class DummyChannel:
    def __init__(self, location):
        self.location = location

    def close(self):
        pass


class DummyFrontend:
    def __init__(self):
        pass


def test_connection_connect_http(monkeypatch):
    # Patch grpc module to avoid real network
    import grpc

    monkeypatch.setattr(grpc, "insecure_channel", lambda loc: DummyChannel(loc))

    class DummyFuture:
        def result(self, timeout=None):
            return None

    monkeypatch.setattr(grpc, "channel_ready_future", lambda ch: DummyFuture())
    monkeypatch.setattr(grpc, "secure_channel", lambda loc, creds=None: DummyChannel(loc))
    monkeypatch.setattr(grpc, "ssl_channel_credentials", lambda root_certificates=None: b"certs")
    monkeypatch.setattr("flamepy.core.client.FrontendStub", lambda channel: DummyFrontend())

    conn = client.Connection.connect("http://localhost:1234")
    assert isinstance(conn, client.Connection)
    conn.close()


def test_connection_connect_https_with_tls(monkeypatch, tmp_path):
    import grpc

    monkeypatch.setattr(grpc, "insecure_channel", lambda loc: DummyChannel(loc))

    class DummyFuture:
        def result(self, timeout=None):
            return None

    monkeypatch.setattr(grpc, "channel_ready_future", lambda ch: DummyFuture())
    monkeypatch.setattr(grpc, "secure_channel", lambda loc, creds=None: DummyChannel(loc))
    called = {"ok": False}

    def fake_ssl_credentials(*args, **kwargs):
        called["ok"] = True
        return b"certs"

    monkeypatch.setattr(grpc, "ssl_channel_credentials", fake_ssl_credentials)
    monkeypatch.setattr("flamepy.core.client.FrontendStub", lambda channel: DummyFrontend())

    tls = client.FlameClientTls(ca_file=str(tmp_path / "ca.pem"))
    # create temp ca file
    (tmp_path / "ca.pem").write_text("CERT")
    tls.ca_file = str(tmp_path / "ca.pem")
    conn = client.Connection.connect("https://localhost:1234", tls_config=tls)
    assert isinstance(conn, client.Connection)
    assert called["ok"]
    conn.close()


def test_session_create_task_with_mocked_frontend(monkeypatch):

    class DummyFrontend:
        def CreateTask(self, req):  # noqa: N802 - matches gRPC stub name
            class Resp:
                metadata = type("M", (), {"id": "tid-1"})
                status = type("S", (), {"state": 0, "creation_time": int(time.time() * 1000), "completion_time": int(time.time() * 1000), "events": []})()

            return Resp()

    class DummyConnection:
        def __init__(self):
            self._frontend = DummyFrontend()
            import concurrent.futures

            self._executor = concurrent.futures.ThreadPoolExecutor(max_workers=2)

        def close(self):
            pass

    fake_conn = DummyConnection()
    from flamepy.core.client import Session, SessionState

    s = Session(connection=fake_conn, id="sess-1", application="app", slots=1, state=SessionState.OPEN, creation_time=datetime.now(timezone.utc), pending=0, running=0, succeed=0, failed=0, completion_time=None)

    t = s.create_task(b"input")
    assert t.session_id == s.id
    assert t.id is not None
