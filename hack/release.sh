#!/usr/bin/env bash

set -euo pipefail

validate_metadata() {
    local release_tag="$1"
    local output="$2"
    local cargo_metadata cargo_version macros_version python_version stdng_version
    local flamepy_version rust_stdng_version rust_macros_version
    local expected_cargo expected_python source_sha

    [[ "${release_tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc[0-9]+)?$ ]] || {
        echo "invalid release tag: ${release_tag}" >&2
        return 1
    }

    cargo_metadata="$(cargo metadata --format-version 1 --no-deps \
        --manifest-path sdk/rust/Cargo.toml)"
    cargo_version="$(jq -er '.packages[] | select(.name == "flame-rs") | .version' \
        <<<"${cargo_metadata}")"
    macros_version="$(jq -er '.packages[] | select(.name == "flame-rs-macros") | .version' \
        <<<"${cargo_metadata}")"
    stdng_version="$(jq -er '.packages[] | select(.name == "stdng") | .version' \
        <<<"${cargo_metadata}")"
    rust_stdng_version="$(jq -er '.packages[] | select(.name == "flame-rs") | .dependencies[] | select(.name == "stdng") | .req' \
        <<<"${cargo_metadata}")"
    rust_macros_version="$(jq -er '.packages[] | select(.name == "flame-rs") | .dependencies[] | select(.name == "flame-rs-macros") | .req' \
        <<<"${cargo_metadata}")"
    python_version="$(cd sdk/python && uv version --short)"
    flamepy_version="$(shell_string_value sdk/python/src/flamepy/__init__.py __version__)"

    [[ "${flamepy_version}" == "${python_version}" ]] || {
        echo "flamepy runtime __version__ does not match pyproject.toml: ${flamepy_version} != ${python_version}" >&2
        return 1
    }
    [[ "${rust_stdng_version}" == "^${stdng_version}" ]] || {
        echo "flame-rs stdng dependency version does not match stdng package version" >&2
        return 1
    }
    [[ "${rust_macros_version}" == "^${macros_version}" ]] || {
        echo "flame-rs-macros dependency version does not match macros package version" >&2
        return 1
    }

    expected_cargo="${release_tag#v}"
    expected_python="${expected_cargo/-rc/rc}"
    [[ "${cargo_version}" == "${expected_cargo}" ]] || {
        echo "flame-rs version ${cargo_version} does not match release tag ${release_tag}" >&2
        return 1
    }
    [[ "${macros_version}" == "${cargo_version}" ]] || {
        echo "flame-rs-macros version ${macros_version} does not match flame-rs ${cargo_version}" >&2
        return 1
    }
    [[ "${python_version}" == "${expected_python}" ]] || {
        echo "flamepy version ${python_version} does not match release tag ${release_tag}; expected ${expected_python}" >&2
        return 1
    }

    source_sha="$(git rev-parse HEAD)"
    {
        printf 'cargo-version=%s\n' "${cargo_version}"
        printf 'docker-tag=%s\n' "${release_tag}"
        printf 'python-version=%s\n' "${python_version}"
        printf 'source-sha=%s\n' "${source_sha}"
        printf 'stdng-version=%s\n' "${stdng_version}"
    } >>"${output}"
}

shell_string_value() {
    local file="$1"
    local variable="$2"
    awk -v variable="${variable}" '
        $0 ~ "^[[:space:]]*" variable "[[:space:]]*=" {
            value = $0
            sub(/^[^"]*"/, "", value)
            sub(/".*$/, "", value)
            print value
            exit
        }
    ' "${file}"
}

require_variable() {
    local name="$1"
    if [[ -z "${!name:-}" ]]; then
        echo "::error::${name} is required" >&2
        return 1
    fi
}

validate_credentials() {
    case "$1" in
        pypi)
            require_variable PYPI_API_TOKEN
            ;;
        crates)
            require_variable CARGO_REGISTRY_TOKEN
            ;;
        docker)
            require_variable DOCKER_HUB_USERNAME
            require_variable DOCKER_HUB_PAT
            ;;
        *)
            echo "unknown credential type: $1" >&2
            return 2
            ;;
    esac
}

build_python() {
    local python_version="$1"
    local dist_dir="dist/flamepy"
    local venv_dir="/tmp/flamepy-release-check"

    (
        cd sdk/python
        uv build --out-dir "../../${dist_dir}"
    )
    uvx --from twine twine check "${dist_dir}"/*

    local wheel
    wheel="$(find "${dist_dir}" -maxdepth 1 -name '*.whl' -print -quit)"
    test -n "${wheel}"
    uv venv --clear --python "$(command -v python)" "${venv_dir}"
    uv pip install --python "${venv_dir}/bin/python" "${wheel}"
    verify_installed_python_package "${venv_dir}" "${python_version}"
}

verify_pypi() {
    local python_version="$1"
    local target_dir="/tmp/flamepy-pypi-check"
    for attempt in $(seq 1 30); do
        rm -rf "${target_dir}"
        if uv pip install --system --target "${target_dir}" \
            "flamepy==${python_version}" && \
            verify_installed_python_package "${target_dir}" "${python_version}"; then
            return 0
        fi
        echo "flamepy ${python_version} is not available yet; retrying (${attempt}/30)"
        sleep 10
    done
    echo "flamepy ${python_version} did not become available on PyPI" >&2
    return 1
}

verify_installed_python_package() {
    local root="$1"
    local expected_version="$2"
    local metadata_dir metadata init_file entrypoint package_version runtime_version

    metadata_dir="$(find "${root}" -type d \
        -name "flamepy-${expected_version}.dist-info" -print -quit)"
    init_file="$(find "${root}" -type d -name flamepy -print -quit)/__init__.py"
    metadata="${metadata_dir}/METADATA"
    entrypoint="$(find "${root}" -type f -name flamepy-runner-e2e -print -quit)"
    [[ -f "${metadata}" && -f "${init_file}" && -x "${entrypoint}" ]] || {
        echo "installed flamepy metadata, package, or entry point is missing under ${root}" >&2
        return 1
    }

    package_version="$(awk '/^Version: / { print $2; exit }' "${metadata}")"
    runtime_version="$(shell_string_value "${init_file}" __version__)"
    [[ "${package_version}" == "${expected_version}" ]] || {
        echo "installed flamepy metadata version ${package_version} does not match ${expected_version}" >&2
        return 1
    }
    [[ "${runtime_version}" == "${expected_version}" ]] || {
        echo "installed flamepy runtime version ${runtime_version} does not match ${expected_version}" >&2
        return 1
    }
    PYTHONPATH="${root}" "${entrypoint}" --help >/dev/null
    echo "verified flamepy ${expected_version} under ${root}"
}

crate_exists() {
    local crate="$1"
    local version="$2"
    local user_agent="$3"
    local status
    status="$(curl --retry 5 --retry-all-errors -sS \
        -A "${user_agent}" \
        -o /dev/null \
        -w '%{http_code}' \
        "https://crates.io/api/v1/crates/${crate}/${version}")"
    case "${status}" in
        200) return 0 ;;
        404) return 1 ;;
        *)
            echo "unexpected crates.io response for ${crate} ${version}: HTTP ${status}" >&2
            return 2
            ;;
    esac
}

wait_for_crate() {
    local crate="$1"
    local version="$2"
    local user_agent="$3"
    local status
    for attempt in $(seq 1 30); do
        if crate_exists "${crate}" "${version}" "${user_agent}"; then
            echo "${crate} ${version} is available on crates.io"
            return 0
        else
            status=$?
            if [[ "${status}" -ne 1 ]]; then
                return "${status}"
            fi
        fi
        echo "${crate} ${version} is not available yet; retrying (${attempt}/30)"
        sleep 10
    done
    echo "${crate} ${version} did not become available on crates.io" >&2
    return 1
}

wait_for_cargo_index() {
    local crate="$1"
    local version="$2"
    for attempt in $(seq 1 30); do
        if cargo info "${crate}@${version}" >/dev/null 2>&1; then
            echo "${crate} ${version} is available in the Cargo index"
            return 0
        fi
        echo "${crate} ${version} is not in the Cargo index yet; retrying (${attempt}/30)"
        sleep 10
    done
    echo "${crate} ${version} did not become available in the Cargo index" >&2
    return 1
}

publish_crate() {
    local crate="$1"
    local version="$2"
    local manifest="$3"
    local user_agent="$4"
    local status
    shift 4

    if crate_exists "${crate}" "${version}" "${user_agent}"; then
        echo "${crate} ${version} is already published; skipping publish"
    else
        status=$?
        if [[ "${status}" -ne 1 ]]; then
            return "${status}"
        fi
        cargo publish --manifest-path "${manifest}" \
            --token "${CARGO_REGISTRY_TOKEN}" "$@"
    fi

    wait_for_crate "${crate}" "${version}" "${user_agent}"
    wait_for_cargo_index "${crate}" "${version}"
}

publish_rust() {
    local cargo_version="$1"
    local stdng_version="$2"
    local user_agent="flame-release-workflow/${GITHUB_REPOSITORY:-xflops/flame}@${GITHUB_SHA:-local}"
    require_variable CARGO_REGISTRY_TOKEN

    cargo package --manifest-path stdng/Cargo.toml
    publish_crate stdng "${stdng_version}" stdng/Cargo.toml "${user_agent}"

    cargo package --manifest-path sdk/rust/macros/Cargo.toml
    publish_crate flame-rs-macros "${cargo_version}" \
        sdk/rust/macros/Cargo.toml "${user_agent}"

    cargo package --manifest-path sdk/rust/Cargo.toml --features macros
    publish_crate flame-rs "${cargo_version}" sdk/rust/Cargo.toml \
        "${user_agent}" --features macros
}

verify_images() {
    if [[ "$#" -eq 0 ]]; then
        echo "verify-images requires at least one image reference" >&2
        return 2
    fi
    local image manifest
    for image in "$@"; do
        manifest="$(docker buildx imagetools inspect "${image}")"
        printf '%s\n' "${manifest}"
        grep -q 'linux/amd64' <<<"${manifest}"
        grep -q 'linux/arm64' <<<"${manifest}"
    done
}

promote_images() {
    local registry="$1"
    local source_sha="$2"
    local images=(
        flame-session-manager
        flame-object-cache
        flame-executor-manager
        flame-console
    )
    local refs=() image repository source_ref latest_ref

    for image in "${images[@]}"; do
        refs+=("${registry}/${image}:sha-${source_sha}")
    done
    verify_images "${refs[@]}"

    for image in "${images[@]}"; do
        repository="${registry}/${image}"
        source_ref="${repository}:sha-${source_sha}"
        latest_ref="${repository}:latest"
        promote_image "${source_ref}" "${latest_ref}"
    done

    refs=()
    for image in "${images[@]}"; do
        refs+=("${registry}/${image}:latest")
    done
    verify_images "${refs[@]}"
}

image_digest() {
    local image="$1"
    docker buildx imagetools inspect "${image}" |
        awk '$1 == "Digest:" && !found { print $2; found = 1 }'
}

promote_image() {
    local source_ref="$1"
    local latest_ref="$2"
    local source_digest latest_digest

    source_digest="$(image_digest "${source_ref}")"
    [[ -n "${source_digest}" ]] || {
        echo "could not resolve manifest digest for ${source_ref}" >&2
        return 1
    }

    for attempt in $(seq 1 5); do
        if docker buildx imagetools create \
            --tag "${latest_ref}" "${source_ref}"; then
            latest_digest=""
            if latest_digest="$(image_digest "${latest_ref}")" && \
                [[ "${latest_digest}" == "${source_digest}" ]]; then
                echo "promoted ${latest_ref} to ${source_digest}"
                return 0
            fi
            echo "${latest_ref} digest ${latest_digest:-missing} does not match ${source_digest}" >&2
        fi
        echo "promotion failed; retrying (${attempt}/5)" >&2
        sleep $((attempt * 5))
    done
    echo "failed to promote ${latest_ref} from ${source_ref}" >&2
    return 1
}

usage() {
    cat >&2 <<'EOF'
Usage:
  hack/release.sh validate-metadata RELEASE_TAG OUTPUT
  hack/release.sh validate-credentials {pypi|crates|docker}
  hack/release.sh build-python PYTHON_VERSION
  hack/release.sh verify-pypi PYTHON_VERSION
  hack/release.sh publish-rust CARGO_VERSION STDNG_VERSION
  hack/release.sh verify-images IMAGE[:TAG]...
  hack/release.sh promote-images IMAGE_REGISTRY SOURCE_SHA
EOF
}

command="${1:-}"
if [[ -z "${command}" ]]; then
    usage
    exit 2
fi
shift

case "${command}" in
    validate-metadata)
        [[ "$#" -eq 2 ]] || { usage; exit 2; }
        validate_metadata "$@"
        ;;
    validate-credentials)
        [[ "$#" -eq 1 ]] || { usage; exit 2; }
        validate_credentials "$@"
        ;;
    build-python)
        [[ "$#" -eq 1 ]] || { usage; exit 2; }
        build_python "$@"
        ;;
    verify-pypi)
        [[ "$#" -eq 1 ]] || { usage; exit 2; }
        verify_pypi "$@"
        ;;
    publish-rust)
        [[ "$#" -eq 2 ]] || { usage; exit 2; }
        publish_rust "$@"
        ;;
    verify-images)
        verify_images "$@"
        ;;
    promote-images)
        [[ "$#" -eq 2 ]] || { usage; exit 2; }
        promote_images "$@"
        ;;
    *)
        usage
        exit 2
        ;;
esac
