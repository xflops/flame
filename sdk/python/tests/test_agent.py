import cloudpickle

import flamepy.agent.client as agent_client


class FakeSession:
    def __init__(self):
        self.application = "myapp"
        self.id = "sess-1"

    def invoke(self, input_bytes):
        return cloudpickle.dumps("OK")

    def close(self):
        pass


def test_agent_init_and_invoke(monkeypatch):
    # Patch create_session to return fake session
    monkeypatch.setattr(agent_client, "create_session", lambda **kwargs: FakeSession())
    a = agent_client.Agent(name="myapp")
    # Patch the session to return a known value on invoke
    result = a.invoke("hello")
    assert result == "OK"


def test_cloudpickle_serialization_of_callable():
    def f(x):
        return x * 2

    s = cloudpickle.dumps(f)
    f2 = cloudpickle.loads(s)
    assert f2(3) == 6
