"""Importing the SDK does not import the heavy data libraries or web frameworks."""

import subprocess
import sys
import tempfile
from pathlib import Path

SRC = str(Path(__file__).resolve().parents[1] / "src")
HEAVY = ("numpy", "pandas", "polars", "pyarrow", "fastapi", "uvicorn")


def run(code: str, *args: str) -> subprocess.CompletedProcess:
    """Run `code` in a fresh interpreter that can import the SDK."""
    script = f"import sys\nsys.path.insert(0, {SRC!r})\n{code}"
    return subprocess.run([sys.executable, "-I", "-c", script, *args], capture_output=True, text=True, check=True)


def loaded_after(code: str) -> list[str]:
    """The heavy modules a fresh interpreter has imported after running `code`."""
    result = run(f"{code}\nprint(','.join(m for m in {HEAVY!r} if m in sys.modules))")
    return [name for name in result.stdout.strip().split(",") if name]


def test_importing_the_sdk_loads_no_heavy_module():
    for module in ("flamepy", "flamepy.core", "flamepy.app"):
        assert loaded_after(f"import {module}") == [], module


def test_submodules_are_imported_on_first_use():
    code = "import flamepy\nassert 'flamepy.serving' not in sys.modules\nimport flamepy as f\nf.serving\nassert 'flamepy.serving' in sys.modules\nassert f.app.__name__ == 'flamepy.app'"
    assert "fastapi" in loaded_after(code)


def test_unknown_attribute_is_still_an_error():
    code = "import flamepy\ntry:\n    flamepy.not_a_module\nexcept AttributeError:\n    pass\nelse:\n    raise SystemExit('expected AttributeError')"
    assert loaded_after(code) == []


def test_serializing_generic_values_loads_no_heavy_module():
    code = "from flamepy.core import cache\nfor value in ({'a': 1}, [1, 2], b'x', 'text'):\n    data_type, data = cache._encode_object_data(value)\n    assert cache._deserialize_object_data(*cache._type_and_compression(data_type)[:1], cache._decompress_data(data, cache._type_and_compression(data_type)[1])) == value"
    assert loaded_after(code) == []


def test_arrow_values_round_trip_across_processes():
    """A value of a library type is encoded where its library is loaded, and the
    library is imported on demand where it is decoded."""
    encode = "import pyarrow as pa\nfrom flamepy.core import cache\ndata_type, data = cache._encode_object_data(pa.table({'a': [1, 2]}))\nopen(sys.argv[1], 'wb').write(data)\nprint(data_type)"
    decode = "from flamepy.core import cache\nassert 'pyarrow' not in sys.modules\nt, c = cache._type_and_compression(sys.argv[2])\ntable = cache._deserialize_object_data(t, cache._decompress_data(open(sys.argv[1], 'rb').read(), c))\nassert table.to_pydict() == {'a': [1, 2]}\nassert 'pyarrow' in sys.modules"
    with tempfile.TemporaryDirectory() as directory:
        path = str(Path(directory) / "payload")
        data_type = run(encode, path).stdout.strip()
        assert data_type == "arrow.table.zstd"
        run(decode, path, data_type)
