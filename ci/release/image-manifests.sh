#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 4 ]]; then
    echo "usage: $0 check REGISTRY ARCH METADATA | merge REGISTRY TAG DIGEST_DIRECTORY" >&2
    exit 2
fi

mode="$1"
registry="$2"
value="$3"
metadata="$4"
images=(flame-session-manager flame-object-cache flame-executor-manager-runtime flame-executor-manager-devel flame-console flame-test flame-instance-runtime flame-instance-runtime-devel)

# A flavor image shares the repository of its stock image, as in nvidia/cuda: the -devel images are
# <repository>:<tag>-devel, and flame-executor-manager-runtime is the stock flame-executor-manager:<tag>.
repository() {
    case "$1" in
        flame-executor-manager-runtime|flame-executor-manager-devel) printf 'flame-executor-manager' ;;
        flame-instance-runtime-devel) printf 'flame-instance-runtime' ;;
        *) printf '%s' "$1" ;;
    esac
}
variant_tag() {
    if [[ "$1" == *-devel ]]; then printf '%s-devel' "$2"; else printf '%s' "$2"; fi
}

digest() {
    local image="$1" file="$2" result
    result="$(jq -er --arg image "$image" '.[$image]["containerimage.digest"]' "$file")"
    if [[ ! "$result" =~ ^sha256:[0-9a-f]{64}$ ]]; then
        echo "invalid digest for ${image} in ${file}" >&2
        return 1
    fi
    printf '%s' "$result"
}

case "$mode" in
    check)
        if [[ "$value" != amd64 && "$value" != arm64 ]]; then
            echo "unsupported architecture: ${value}" >&2
            exit 1
        fi
        refs=()
        for image in "${images[@]}"; do
            refs+=("${registry}/$(repository "$image")@$(digest "$image" "$metadata")")
        done
        for i in "${!images[@]}"; do
            image="${images[$i]}"
            ref="${refs[$i]}"
            docker pull --platform "linux/${value}" "$ref"
            if [[ "$(docker image inspect --format '{{.Architecture}}' "$ref")" != "$value" ]]; then
                echo "image architecture does not match ${value}: ${ref}" >&2
                exit 1
            fi
            case "$image" in
                flame-session-manager|flame-object-cache|flame-executor-manager-runtime|flame-executor-manager-devel)
                    docker run --rm --platform "linux/${value}" "$ref" --help
                    ;;
                flame-console)
                    docker run --rm --platform "linux/${value}" --entrypoint /usr/local/flame/bin/flmctl "$ref" --help
                    ;;
                flame-test)
                    docker run --rm --platform "linux/${value}" "$ref" flame-test help
                    ;;
                flame-instance-runtime|flame-instance-runtime-devel)
                    docker run --rm --platform "linux/${value}" --entrypoint /usr/local/flame/bin/uv "$ref" --version
                    ;;
            esac
            case "$image" in
                flame-executor-manager-devel|flame-instance-runtime-devel)
                    # Triton compiles a C helper against libc and Python.h with the system compiler.
                    docker run --rm --platform "linux/${value}" --entrypoint /bin/bash "$ref" -lc \
                        'gcc --version >/dev/null && echo "#include <Python.h>" | gcc -E $(python3-config --includes) - >/dev/null'
                    ;;
            esac
            case "$image" in
                flame-executor-manager-runtime|flame-executor-manager-devel|flame-console|flame-test|flame-instance-runtime|flame-instance-runtime-devel)
                    docker run --rm --platform "linux/${value}" --entrypoint /bin/bash "$ref" -lc \
                        'source /usr/local/flame/sbin/flmenv.sh --python-version 3.14 && python3 -c "import flamepy"'
                    ;;
            esac
        done
        ;;
    merge)
        # Validate every digest before writing any final tag.
        amd64=() arm64=() refs=()
        for image in "${images[@]}"; do
            amd64+=("${registry}/$(repository "$image")@$(digest "$image" "${metadata}/digests-amd64.json")")
            arm64+=("${registry}/$(repository "$image")@$(digest "$image" "${metadata}/digests-arm64.json")")
            refs+=("${registry}/$(repository "$image"):$(variant_tag "$image" "$value")")
        done
        for i in "${!images[@]}"; do
            docker buildx imagetools create --tag "${refs[$i]}" "${amd64[$i]}" "${arm64[$i]}"
        done
        root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
        bash "${root}/hack/release.sh" verify-images "${refs[@]}"
        ;;
    *)
        echo "usage: $0 check REGISTRY ARCH METADATA | merge REGISTRY TAG DIGEST_DIRECTORY" >&2
        exit 2
        ;;
esac
