#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

LOCAL_CHECKS="${RELEASE_SANITY_LOCAL_CHECKS:-1}"
PACKAGE_CHECKS="${RELEASE_SANITY_PACKAGE_CHECKS:-1}"
REMOTE_CHECKS="${RELEASE_SANITY_REMOTE_CHECKS:-0}"
K8S_E2E="${RELEASE_SANITY_K8S_E2E:-0}"
COMPOSE_E2E="${RELEASE_SANITY_COMPOSE_E2E:-0}"
METADATA_CHECKS="${RELEASE_SANITY_METADATA_CHECKS:-1}"
EXPECTED_PLATFORMS="${RELEASE_SANITY_EXPECTED_PLATFORMS:-linux/amd64 linux/arm64}"
IMAGE_REPOSITORIES="${RELEASE_SANITY_IMAGE_REPOSITORIES:-flame-session-manager flame-object-cache flame-executor-manager flame-console}"
CONTAINER_CLI="${RELEASE_SANITY_CONTAINER_CLI:-${RELEASE_SANITY_CONTAINER_RUNTIME:-${CONTAINER_RUNTIME:-}}}"
COMPOSE_PROJECT_NAME="${RELEASE_SANITY_COMPOSE_PROJECT_NAME:-flame-release-sanity}"
COMPOSE_NETWORK_NAME="${RELEASE_SANITY_COMPOSE_NETWORK_NAME:-flame-release-sanity-flame-net}"
COMPOSE_PULL="${RELEASE_SANITY_COMPOSE_PULL:-1}"
COMPOSE_DOWN="${RELEASE_SANITY_COMPOSE_DOWN:-1}"
COMPOSE_TIMEOUT_SECONDS="${RELEASE_SANITY_COMPOSE_TIMEOUT_SECONDS:-180}"
COMPOSE_E2E_TASKS="${RELEASE_SANITY_COMPOSE_E2E_TASKS:-1}"
PYPI_INDEX_URL="${RELEASE_SANITY_PYPI_INDEX_URL:-https://pypi.org/simple}"
PYPI_CHECK_IMAGE="${RELEASE_SANITY_PYPI_CHECK_IMAGE:-python:3.12-slim}"
COMPOSE_OVERRIDE_FILE=""
PYPI_CHECK_SCRIPT=""
MANIFEST_FILE=""
COMPOSE_STARTED=0

log() {
    printf '[release-sanity] %s\n' "$*"
}

fail() {
    log "ERROR: $*"
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

enabled() {
    case "$1" in
        1 | true | TRUE | yes | YES | on | ON)
            return 0
            ;;
        0 | false | FALSE | no | NO | off | OFF)
            return 1
            ;;
        *)
            fail "expected boolean value, got: $1"
            ;;
    esac
}

run() {
    log "$*"
    "$@"
}

cleanup() {
    if [ "$COMPOSE_STARTED" = "1" ] && enabled "$COMPOSE_DOWN"; then
        log "Stopping Docker Compose project ${COMPOSE_PROJECT_NAME}"
        compose_command down -v || true
    fi

    if [ -n "$COMPOSE_OVERRIDE_FILE" ]; then
        rm -f "$COMPOSE_OVERRIDE_FILE"
    fi

    if [ -n "$PYPI_CHECK_SCRIPT" ]; then
        rm -f "$PYPI_CHECK_SCRIPT"
    fi

    if [ -n "${MANIFEST_FILE:-}" ]; then
        rm -f "$MANIFEST_FILE"
    fi
}

assert_eq() {
    local label="$1"
    local actual="$2"
    local expected="$3"

    if [ "$actual" != "$expected" ]; then
        fail "${label}: expected ${expected}, got ${actual}"
    fi
}

load_detected_versions() {
    need python3

    local detected_versions

    detected_versions="$(python3 - "$ROOT_DIR" <<'PY'
import ast
import pathlib
import shlex
import sys

try:
    import tomllib
except ModuleNotFoundError as exc:
    raise SystemExit("python3 with tomllib is required") from exc

root = pathlib.Path(sys.argv[1])


def load_toml(path):
    with path.open("rb") as f:
        return tomllib.load(f)


def shell_var(name, value):
    if not value:
        raise SystemExit(f"missing value for {name}")
    print(f"{name}={shlex.quote(str(value))}")


def package_version(path):
    data = load_toml(path)
    return data["package"]["version"]


def pyproject_version(path):
    data = load_toml(path)
    return data["project"]["version"]


def python_init_version(path):
    tree = ast.parse(path.read_text())
    for node in tree.body:
        if not isinstance(node, ast.Assign):
            continue
        for target in node.targets:
            if isinstance(target, ast.Name) and target.id == "__version__":
                if isinstance(node.value, ast.Constant) and isinstance(node.value.value, str):
                    return node.value.value
    raise SystemExit(f"missing __version__ assignment in {path}")


def chart_top_level(path, key):
    prefix = f"{key}:"
    for line in path.read_text().splitlines():
        if line.startswith((" ", "\t", "#")):
            continue
        stripped = line.strip()
        if stripped.startswith(prefix):
            return stripped.split(":", 1)[1].strip().strip("\"'")
    raise SystemExit(f"missing {key} in {path}")


sdk_rust = load_toml(root / "sdk/rust/Cargo.toml")
stdng_dep = sdk_rust["dependencies"]["stdng"]
macros_dep = sdk_rust["dependencies"]["flame-rs-macros"]

shell_var("DETECTED_PYPROJECT_VERSION", pyproject_version(root / "sdk/python/pyproject.toml"))
shell_var("DETECTED_PYTHON_INIT_VERSION", python_init_version(root / "sdk/python/src/flamepy/__init__.py"))
shell_var("DETECTED_FLAME_RS_VERSION", package_version(root / "sdk/rust/Cargo.toml"))
shell_var("DETECTED_FLAME_RS_MACROS_VERSION", package_version(root / "sdk/rust/macros/Cargo.toml"))
shell_var("DETECTED_STDNG_VERSION", package_version(root / "stdng/Cargo.toml"))
shell_var("DETECTED_CHART_APP_VERSION", chart_top_level(root / "charts/flame/Chart.yaml", "appVersion"))
shell_var("DETECTED_FLAME_RS_STDNG_DEP_VERSION", stdng_dep.get("version"))
shell_var("DETECTED_FLAME_RS_MACROS_DEP_VERSION", macros_dep.get("version"))
PY
)"

    eval "$detected_versions"
}

configure_inputs() {
    CARGO_VERSION="${CARGO_VERSION:-$DETECTED_FLAME_RS_VERSION}"
    PYTHON_VERSION="${PYTHON_VERSION:-$DETECTED_PYPROJECT_VERSION}"
    STDNG_VERSION="${STDNG_VERSION:-$DETECTED_STDNG_VERSION}"
    CHART_APP_VERSION="${CHART_APP_VERSION:-$DETECTED_CHART_APP_VERSION}"
    RELEASE_TAG="${RELEASE_TAG:-v${CARGO_VERSION}}"
    DOCKER_TAG="${DOCKER_TAG:-$RELEASE_TAG}"
    IMAGE_REGISTRY="${IMAGE_REGISTRY:-docker.io/xflops}"
    ARTIFACT_DIR="${RELEASE_SANITY_ARTIFACT_DIR:-/tmp/flame-release-sanity/${PYTHON_VERSION}}"

    case "$RELEASE_TAG" in
        v*) ;;
        *) fail "RELEASE_TAG must include the leading v, got: ${RELEASE_TAG}" ;;
    esac

    case "$CARGO_VERSION" in
        v*) fail "CARGO_VERSION must not include the leading v, got: ${CARGO_VERSION}" ;;
    esac

    case "$PYTHON_VERSION" in
        v*) fail "PYTHON_VERSION must not include the leading v, got: ${PYTHON_VERSION}" ;;
        *-rc*) fail "PYTHON_VERSION must use PEP 440 rc spelling, for example 0.6.0rc1" ;;
    esac

    case "$ARTIFACT_DIR" in
        "" | /) fail "unsafe RELEASE_SANITY_ARTIFACT_DIR: ${ARTIFACT_DIR}" ;;
    esac
}

check_metadata() {
    log "Release inputs"
    log "  RELEASE_TAG=${RELEASE_TAG}"
    log "  CARGO_VERSION=${CARGO_VERSION}"
    log "  PYTHON_VERSION=${PYTHON_VERSION}"
    log "  STDNG_VERSION=${STDNG_VERSION}"
    log "  CHART_APP_VERSION=${CHART_APP_VERSION}"
    log "  DOCKER_TAG=${DOCKER_TAG}"
    log "  IMAGE_REGISTRY=${IMAGE_REGISTRY}"
    log "  CONTAINER_CLI=${CONTAINER_CLI:-auto}"
    log "  RELEASE_SANITY_METADATA_CHECKS=${METADATA_CHECKS}"
    log "  RELEASE_SANITY_COMPOSE_E2E=${COMPOSE_E2E}"

    if ! enabled "$METADATA_CHECKS"; then
        log "Skipping metadata assertions"
        return
    fi

    assert_eq "sdk/python project.version" "$DETECTED_PYPROJECT_VERSION" "$PYTHON_VERSION"
    assert_eq "flamepy.__version__" "$DETECTED_PYTHON_INIT_VERSION" "$PYTHON_VERSION"
    assert_eq "flame-rs package.version" "$DETECTED_FLAME_RS_VERSION" "$CARGO_VERSION"
    assert_eq "flame-rs-macros package.version" "$DETECTED_FLAME_RS_MACROS_VERSION" "$CARGO_VERSION"
    assert_eq "stdng package.version" "$DETECTED_STDNG_VERSION" "$STDNG_VERSION"
    assert_eq "charts/flame appVersion" "$DETECTED_CHART_APP_VERSION" "$CHART_APP_VERSION"
    assert_eq "flame-rs stdng dependency version" "$DETECTED_FLAME_RS_STDNG_DEP_VERSION" "$STDNG_VERSION"
    assert_eq "flame-rs-macros dependency version" "$DETECTED_FLAME_RS_MACROS_DEP_VERSION" "$CARGO_VERSION"
}

run_local_checks() {
    need cargo

    run git -C "$ROOT_DIR" diff --check
    run cargo fmt --check
    run cargo check -p stdng
    run cargo check -p flame-rs --features macros
    run bash -n "$ROOT_DIR/ci/k8s/e2e.sh"

    log "python3 -m json.tool charts/flame/values.schema.json"
    python3 -m json.tool "$ROOT_DIR/charts/flame/values.schema.json" >/dev/null

    if command -v helm >/dev/null 2>&1; then
        run helm lint "$ROOT_DIR/charts/flame" \
            --set "global.imageRegistry=${IMAGE_REGISTRY}" \
            --set "global.imageTag=${DOCKER_TAG}"
        log "helm template flame charts/flame"
        helm template flame "$ROOT_DIR/charts/flame" \
            --set "global.imageRegistry=${IMAGE_REGISTRY}" \
            --set "global.imageTag=${DOCKER_TAG}" >/dev/null
    else
        log "Skipping Helm lint/template because helm is not installed"
    fi
}

run_package_checks() {
    need cargo
    need uv

    run cargo package --manifest-path "$ROOT_DIR/stdng/Cargo.toml" --allow-dirty
    run cargo package --manifest-path "$ROOT_DIR/sdk/rust/macros/Cargo.toml" --allow-dirty
    run cargo package --manifest-path "$ROOT_DIR/sdk/rust/Cargo.toml" --allow-dirty --features macros

    log "uv run Python SDK runner tests"
    (
        cd "$ROOT_DIR/sdk/python"
        uv run -n --extra dev pytest tests/test_runner_e2e.py tests/test_runner.py -q
    )

    log "uv run Python SDK version import"
    (
        cd "$ROOT_DIR/sdk/python"
        uv run -n python - "$PYTHON_VERSION" <<'PY'
import sys

import flamepy

expected = sys.argv[1]
actual = flamepy.__version__
print(actual)
if actual != expected:
    raise SystemExit(f"expected {expected}, got {actual}")
PY
    )

    rm -rf "$ARTIFACT_DIR"
    mkdir -p "$ARTIFACT_DIR"
    log "uv build --out-dir ${ARTIFACT_DIR}"
    (
        cd "$ROOT_DIR/sdk/python"
        uv build --out-dir "$ARTIFACT_DIR"
    )
}

check_url() {
    local label="$1"
    local url="$2"

    need curl
    log "Checking ${label}: ${url}"
    curl -fsSL "$url" >/dev/null
}

check_manifest_platforms() {
    local image="$1"
    local manifest_file="$2"

    python3 "$ROOT_DIR/ci/release/check-image-platforms.py" \
        "$image" "$EXPECTED_PLATFORMS" <"$manifest_file"
}

inspect_image_manifest() {
    local image="$1"

    MANIFEST_FILE="$(mktemp "${TMPDIR:-/tmp}/flame-release-sanity-manifest.XXXXXX")"

    log "${CONTAINER_CLI} manifest inspect ${image}"
    if "$CONTAINER_CLI" manifest inspect "$image" >"$MANIFEST_FILE" 2>/dev/null; then
        check_manifest_platforms "$image" "$MANIFEST_FILE"
        rm -f "$MANIFEST_FILE"
        MANIFEST_FILE=""
        return
    fi

    log "${CONTAINER_CLI} pull ${image}"
    "$CONTAINER_CLI" pull "$image" >/dev/null
    log "${CONTAINER_CLI} image inspect ${image}"
    "$CONTAINER_CLI" image inspect "$image" >"$MANIFEST_FILE"
    check_manifest_platforms "$image" "$MANIFEST_FILE"

    rm -f "$MANIFEST_FILE"
    MANIFEST_FILE=""
}

run_remote_checks() {
    check_url "PyPI flamepy ${PYTHON_VERSION}" "https://pypi.org/pypi/flamepy/${PYTHON_VERSION}/json"
    check_url "crates.io stdng ${STDNG_VERSION}" "https://crates.io/api/v1/crates/stdng/${STDNG_VERSION}"
    check_url "crates.io flame-rs-macros ${CARGO_VERSION}" "https://crates.io/api/v1/crates/flame-rs-macros/${CARGO_VERSION}"
    check_url "crates.io flame-rs ${CARGO_VERSION}" "https://crates.io/api/v1/crates/flame-rs/${CARGO_VERSION}"

    select_container_cli
    for repo in $IMAGE_REPOSITORIES; do
        inspect_image_manifest "${IMAGE_REGISTRY}/${repo}:${DOCKER_TAG}"
    done
}

run_k8s_e2e() {
    IMAGE_REGISTRY="$IMAGE_REGISTRY" IMAGE_TAG="$DOCKER_TAG" "$ROOT_DIR/ci/k8s/e2e.sh"
}

select_container_cli() {
    if [ -n "$CONTAINER_CLI" ]; then
        need "$CONTAINER_CLI"
        "$CONTAINER_CLI" compose version >/dev/null 2>&1 || fail "${CONTAINER_CLI} compose is not available"
        return
    fi

    local candidate
    for candidate in docker podman; do
        if command -v "$candidate" >/dev/null 2>&1 && "$candidate" compose version >/dev/null 2>&1; then
            CONTAINER_CLI="$candidate"
            return
        fi
    done

    fail "docker or podman with compose support is required for container sanity checks"
}

compose_command() {
    "$CONTAINER_CLI" compose \
        -p "$COMPOSE_PROJECT_NAME" \
        -f "$ROOT_DIR/compose.yaml" \
        -f "$COMPOSE_OVERRIDE_FILE" \
        "$@"
}

write_compose_override() {
    COMPOSE_OVERRIDE_FILE="$(mktemp "${TMPDIR:-/tmp}/flame-release-sanity-compose.XXXXXX")"
    cat >"$COMPOSE_OVERRIDE_FILE" <<EOF
services:
  flame-session-manager:
    image: ${IMAGE_REGISTRY}/flame-session-manager:${DOCKER_TAG}
  flame-object-cache:
    image: ${IMAGE_REGISTRY}/flame-object-cache:${DOCKER_TAG}
  flame-executor-manager:
    image: ${IMAGE_REGISTRY}/flame-executor-manager:${DOCKER_TAG}
  flame-console:
    image: ${IMAGE_REGISTRY}/flame-console:${DOCKER_TAG}
networks:
  flame-net:
    name: ${COMPOSE_NETWORK_NAME}
EOF
}

write_pypi_check_script() {
    PYPI_CHECK_SCRIPT="$(mktemp "${TMPDIR:-/tmp}/flame-release-sanity-pypi-check.XXXXXX")"
    cat >"$PYPI_CHECK_SCRIPT" <<'EOF'
#!/bin/sh
set -eu

unset FLAME_HOME
unset PYTHONHOME
unset PYTHONPATH
export PYTHONNOUSERSITE=1
export PIP_CONFIG_FILE=/dev/null

python -m pip install --no-cache-dir --index-url "${PYPI_INDEX_URL}" "flamepy==${PYTHON_VERSION}"

python - "${PYTHON_VERSION}" <<'PY'
import pathlib
import sys

import flamepy

expected = sys.argv[1]
actual = flamepy.__version__
source = pathlib.Path(flamepy.__file__).resolve()
print(f"flamepy {actual} from {source}")

if actual != expected:
    raise SystemExit(f"expected flamepy {expected}, got {actual}")

if "/usr/local/flame" in str(source):
    raise SystemExit(f"imported preinstalled flamepy from {source}")
PY

cd /tmp
python -m flamepy.runner.e2e --tasks "${RUNNER_E2E_TASKS}" --json
EOF
    chmod +x "$PYPI_CHECK_SCRIPT"
}

show_compose_state() {
    compose_command ps || true
    compose_command logs --tail=160 || true
}

wait_for_compose_cluster() {
    local deadline
    deadline=$((SECONDS + COMPOSE_TIMEOUT_SECONDS))

    log "Waiting for compose cluster flmrun template"
    while true; do
        if compose_command exec -T flame-console flmctl --config /root/.flame/flame.yaml list -a 2>/dev/null | grep -q flmrun; then
            log "Compose cluster is ready"
            return 0
        fi

        if [ "$SECONDS" -ge "$deadline" ]; then
            show_compose_state
            fail "compose cluster did not become ready within ${COMPOSE_TIMEOUT_SECONDS}s"
        fi

        sleep 5
    done
}

run_pypi_compose_e2e() {
    log "Running PyPI flamepy ${PYTHON_VERSION} Runner check in ${PYPI_CHECK_IMAGE}"
    run "$CONTAINER_CLI" run --rm \
        --network "$COMPOSE_NETWORK_NAME" \
        -v "$ROOT_DIR/ci/certs:/etc/flame/certs:ro" \
        -v "$PYPI_CHECK_SCRIPT:/tmp/flame-release-sanity-pypi-check.sh:ro" \
        -e "PYPI_INDEX_URL=${PYPI_INDEX_URL}" \
        -e "PYTHON_VERSION=${PYTHON_VERSION}" \
        -e "RUNNER_E2E_TASKS=${COMPOSE_E2E_TASKS}" \
        -e "FLAME_ENDPOINT=https://flame-session-manager:8080" \
        -e "FLAME_CACHE_ENDPOINT=grpcs://flame-object-cache:9090" \
        -e "FLAME_CA_FILE=/etc/flame/certs/ca.crt" \
        "$PYPI_CHECK_IMAGE" \
        /bin/sh /tmp/flame-release-sanity-pypi-check.sh
}

run_compose_e2e() {
    select_container_cli
    write_compose_override
    write_pypi_check_script

    log "Docker Compose release images"
    log "  ${IMAGE_REGISTRY}/flame-session-manager:${DOCKER_TAG}"
    log "  ${IMAGE_REGISTRY}/flame-object-cache:${DOCKER_TAG}"
    log "  ${IMAGE_REGISTRY}/flame-executor-manager:${DOCKER_TAG}"
    log "  ${IMAGE_REGISTRY}/flame-console:${DOCKER_TAG}"

    if enabled "$COMPOSE_PULL"; then
        run compose_command pull
    else
        log "Skipping compose image pull"
    fi

    run compose_command up -d --no-build
    COMPOSE_STARTED=1

    wait_for_compose_cluster
    run_pypi_compose_e2e
}

main() {
    if [ "$#" -ne 0 ]; then
        fail "no positional arguments are supported; configure with environment variables"
    fi

    load_detected_versions
    configure_inputs
    check_metadata
    trap cleanup EXIT

    if enabled "$LOCAL_CHECKS"; then
        run_local_checks
    else
        log "Skipping local checks"
    fi

    if enabled "$PACKAGE_CHECKS"; then
        run_package_checks
    else
        log "Skipping package checks"
    fi

    if enabled "$REMOTE_CHECKS"; then
        run_remote_checks
    else
        log "Skipping remote artifact checks"
    fi

    if enabled "$K8S_E2E"; then
        run_k8s_e2e
    else
        log "Skipping Kubernetes e2e"
    fi

    if enabled "$COMPOSE_E2E"; then
        run_compose_e2e
    else
        log "Skipping Docker Compose e2e"
    fi

    log "Release sanity completed for ${RELEASE_TAG}"
}

main "$@"
