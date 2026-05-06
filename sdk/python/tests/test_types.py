import json
from datetime import datetime, timezone

from flamepy.core.types import (
    Application,
    ApplicationAttributes,
    ApplicationSchema,
    ApplicationState,
    Event,
    FlameContext,
    FlameError,
    FlameErrorCode,
    ResourceRequirement,
    SessionAttributes,
    SessionState,
    Shim,
    Task,
    TaskState,
    short_name,
)


def test_enums_and_flame_error():
    # Enums should be int-like and have expected values
    assert int(SessionState.OPEN) == 0
    assert int(TaskState.PENDING) == 0
    assert int(ApplicationState.ENABLED) == 0
    assert int(Shim.HOST) == 0
    assert int(FlameErrorCode.INVALID_ARGUMENT) == 2

    # FlameError
    err = FlameError(FlameErrorCode.INVALID_ARGUMENT, "bad arg")
    assert err.code == FlameErrorCode.INVALID_ARGUMENT
    assert "bad arg" in str(err)


def test_dataclass_defaults_and_instantiation():
    t = Event(code=1)
    sa = SessionAttributes(application="app", slots=2)
    ap_schema = ApplicationSchema()
    ap_attrs = ApplicationAttributes()
    dt = datetime.now(timezone.utc)
    task = Task(id="tid", session_id="sid", state=TaskState.PENDING, creation_time=dt)
    app = Application(id="aid", name="n", state=ApplicationState.ENABLED, creation_time=dt)
    assert t.code == 1
    assert sa.application == "app" and sa.slots == 2
    assert ap_schema.input is None
    assert ap_attrs.image is None
    assert task.input is None
    assert app.name == "n"


def test_resource_requirement_defaults():
    """Test ResourceRequirement with default values."""
    rr = ResourceRequirement()
    assert rr.cpu == 0
    assert rr.memory == 0
    assert rr.gpu == 0


def test_resource_requirement_explicit_values():
    """Test ResourceRequirement with explicit values."""
    rr = ResourceRequirement(cpu=4, memory=8 * 1024**3, gpu=2)
    assert rr.cpu == 4
    assert rr.memory == 8 * 1024**3
    assert rr.gpu == 2


def test_resource_requirement_from_string_full():
    """Test parsing resource requirements from full string."""
    rr = ResourceRequirement.from_string("cpu=4,mem=16g,gpu=2")
    assert rr.cpu == 4
    assert rr.memory == 16 * 1024**3
    assert rr.gpu == 2


def test_resource_requirement_from_string_partial():
    """Test parsing resource requirements with only some fields."""
    rr = ResourceRequirement.from_string("cpu=8")
    assert rr.cpu == 8
    assert rr.memory == 0
    assert rr.gpu == 0


def test_resource_requirement_from_string_memory_variants():
    """Test parsing different memory unit formats."""
    # Kilobytes
    rr_k = ResourceRequirement.from_string("memory=1024k")
    assert rr_k.memory == 1024 * 1024

    # Megabytes
    rr_m = ResourceRequirement.from_string("mem=512m")
    assert rr_m.memory == 512 * 1024**2

    # Gigabytes
    rr_g = ResourceRequirement.from_string("mem=8g")
    assert rr_g.memory == 8 * 1024**3

    # Plain bytes
    rr_bytes = ResourceRequirement.from_string("memory=1048576")
    assert rr_bytes.memory == 1048576


def test_resource_requirement_from_string_with_spaces():
    """Test parsing with extra whitespace."""
    rr = ResourceRequirement.from_string("  cpu = 2 , mem = 4g , gpu = 1 ")
    assert rr.cpu == 2
    assert rr.memory == 4 * 1024**3
    assert rr.gpu == 1


def test_resource_requirement_parse_memory_empty():
    """Test _parse_memory with empty string."""
    assert ResourceRequirement._parse_memory("") == 0
    assert ResourceRequirement._parse_memory("  ") == 0


def test_session_attributes_with_resreq():
    """Test SessionAttributes with resource requirements."""
    rr = ResourceRequirement(cpu=4, memory=8 * 1024**3, gpu=1)
    sa = SessionAttributes(application="test-app", resreq=rr)
    assert sa.application == "test-app"
    assert sa.slots == 0  # default when resreq is used
    assert sa.resreq is not None
    assert sa.resreq.cpu == 4
    assert sa.resreq.memory == 8 * 1024**3
    assert sa.resreq.gpu == 1


def test_session_attributes_without_resreq():
    """Test SessionAttributes without resource requirements (uses slots)."""
    sa = SessionAttributes(application="test-app", slots=4)
    assert sa.application == "test-app"
    assert sa.slots == 4
    assert sa.resreq is None


def test_application_attributes_with_installer():
    """Test ApplicationAttributes with installer field."""
    attrs = ApplicationAttributes(
        image="my-image:latest",
        command="/usr/bin/app",
        installer="pip install mypackage",
    )
    assert attrs.image == "my-image:latest"
    assert attrs.command == "/usr/bin/app"
    assert attrs.installer == "pip install mypackage"


def test_application_with_installer():
    """Test Application dataclass with installer field."""
    dt = datetime.now(timezone.utc)
    app = Application(
        id="app-1",
        name="test-app",
        state=ApplicationState.ENABLED,
        creation_time=dt,
        installer="curl -sSL https://install.sh | bash",
    )
    assert app.installer == "curl -sSL https://install.sh | bash"


def test_short_name_generation():
    s1 = short_name("foo", length=8)
    s2 = short_name("bar", length=8)
    assert s1.startswith("foo-")
    assert s2.startswith("bar-")
    assert len(s1) >= len("foo-") + 8
    assert len(s2) >= len("bar-") + 8


def test_flame_context_env_overrides(tmp_path, monkeypatch):
    # Build a fake flame.yaml in a temp home and override with env vars
    fake_home = tmp_path / ".home"
    fake_home.mkdir()
    # Monkeypatch Path.home() via env var in FlameContext by setting HOME to tmp
    monkeypatch.setenv("HOME", str(fake_home))

    flame_yaml = {
        "current-context": "flame",
        "contexts": [
            {
                "name": "flame",
                "cluster": {"endpoint": "http://localhost:8080"},
            }
        ],
    }
    conf_dir = fake_home / ".flame"
    conf_dir.mkdir()
    (conf_dir / "flame.yaml").write_text(json.dumps(flame_yaml))

    # No env override: endpoint should come from config
    ctx = FlameContext()
    assert ctx.endpoint == "http://localhost:8080"

    # Override with FLAME_ENDPOINT
    monkeypatch.setenv("FLAME_ENDPOINT", "http://override:1234")
    ctx2 = FlameContext()
    assert ctx2.endpoint == "http://override:1234"
