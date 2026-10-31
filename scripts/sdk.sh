#!/bin/sh
# Install the Flame client: the tools flmctl, flmping, flmexec and flmbench, and the Python SDK flamepy.
#
#   curl -fsSL https://xflops.io/flame/sdk.sh | sh
#   curl -fsSL https://xflops.io/flame/sdk.sh | sh -s -- --prefix /opt/flame-client --version v0.7.0
#
# See docs/designs/RFE683-script-install/FS.md. POSIX sh; no root needed for the default prefix.
set -eu

FLAME_LIB_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/lib" || FLAME_LIB_DIR=/nonexistent
. "$FLAME_LIB_DIR/common.sh" #@include lib/common.sh

usage() {
    cat <<'USAGE'
usage: sdk.sh [--prefix DIR] [--version TAG] [--python-version V]... [--force]

  --prefix DIR         where bin/, lib/ and sbin/flmenv.sh go (default $HOME/.flame)
  --version TAG        the release to install (default: the package this script is in, else the
                       release this script was published with, else the latest; or FLAME_VERSION)
  --python-version V   Python version to install flamepy for (repeatable; default 3.11 3.12 3.13 3.14);
                       flmenv.sh selects the one that matches the python3 you run
  --force              replace a client that is already there without asking (without it, sdk.sh asks,
                       and with no terminal to ask on, stops)

environment: FLAME_VERSION, FLAME_BASE_URL (release download base, for a mirror)

sdk.sh writes PREFIX/flame.yaml only from FLAME_ENDPOINT (and FLAME_CACHE_ENDPOINT), set in the
environment or in ./.env (the environment wins). There are no defaults: with no FLAME_ENDPOINT it writes
no configuration, and the tools report what is missing.
USAGE
}

prefix="${HOME:-}/.flame"
version=""
py_versions=""
force=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix="$2"; shift 2 ;;
        --version) [ $# -ge 2 ] || die "--version needs a tag"; version="$2"; shift 2 ;;
        --python-version) [ $# -ge 2 ] || die "--python-version needs a version"; py_versions="$py_versions $2"; shift 2 ;;
        --force) force=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done
case "$prefix" in /*) ;; *) die "--prefix must be an absolute path (is HOME set?)" ;; esac
[ -n "$py_versions" ] || py_versions="3.11 3.12 3.13 3.14"

# install_flamepy SPEC: install flamepy for every requested Python that exists and set $installed.
install_flamepy() {
    installed=""
    for v in $py_versions; do
        log "  installing for Python $v..."
        if have uv; then
            if output="$(uv pip install --python "python$v" --prefix "$prefix" "$1" 2>&1)"; then
                installed="$installed $v"
            else
                case "$output" in
                    *"No interpreter found"* | *"not found"* | *"Cannot find"*)
                        warn "Python $v is not available, skipping" ;;
                    *) die "failed to install $1 for Python $v: $output" ;;
                esac
            fi
        elif have "python$v"; then
            "python$v" -m pip --version >/dev/null 2>&1 \
                || die "pip is required to install $1: install uv (https://astral.sh/uv) or pip for python$v"
            "python$v" -m pip install --prefix "$prefix" "$1" || die "failed to install $1 for Python $v"
            installed="$installed $v"
        else
            warn "Python $v is not available, skipping"
        fi
    done
    installed="${installed# }"
    [ -n "$installed" ] || die "failed to install $1: no Python interpreter found for $py_versions"
}

main() {
    load_dotenv ./.env
    check_settings FLAME_ENDPOINT FLAME_CACHE_ENDPOINT
    # run as <package>/sbin/sdk.sh, it installs the tools and the SDK the package carries
    local_package=""
    if own="$(own_package)" && [ -z "$version" ] && [ -z "${FLAME_VERSION:-}" ]; then
        local_package="$own"
        check_package_format "$local_package"
        tag="$(cat "$local_package/VERSION")"
    else
        detect_platform
        tag="$(resolve_tag "$version")"
        base="$(release_base "$tag")"
        suffix="$tag-$FLAME_OS-$FLAME_ARCH"
    fi

    # the client configuration is written only when FLAME_ENDPOINT says where the cluster is
    conf=""
    [ -z "$(setting FLAME_ENDPOINT "")" ] || conf="$prefix/flame.yaml"

    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT INT TERM
    if [ "$force" -ne 1 ]; then
        for file in "$prefix/bin/flmctl" "$prefix/bin/flmping" "$prefix/bin/flmexec" "$prefix/bin/flmbench" "$prefix/sbin/flmenv.sh" "$conf"; do
            [ -n "$file" ] && [ -e "$file" ] || continue
            log "$file already exists: there is a Flame client at $prefix."
            status=0
            confirm "Overwrite the existing client?" || status=$?
            case "$status" in
                0) break ;;
                1) die "not overwriting $file; nothing was changed" ;;
                *) die "$file already exists and there is no terminal to ask on; use --force to replace the client" ;;
            esac
        done
    fi
    mkdir -p "$prefix/bin" "$prefix/sbin" \
        || die "cannot create $prefix: pass --prefix with a directory you can write"

    if [ -n "$local_package" ]; then
        log "Using the client tools in $local_package..."
        for tool in flmctl flmping flmexec flmbench; do
            [ -f "$local_package/bin/$tool" ] || die "the package has no bin/$tool"
            cp "$local_package/bin/$tool" "$work/$tool"
        done
    else
        log "Downloading Flame $tag client tools..."
        for tool in flmctl flmping flmexec flmbench; do
            fetch_verified "$base/$tool-$suffix" "$work/$tool"
        done
    fi
    # every tool is verified before any is installed
    for tool in flmctl flmping flmexec flmbench; do
        install -m 0755 "$work/$tool" "$prefix/bin/$tool"
        log "  installed $prefix/bin/$tool"
    done

    if [ -n "$local_package" ]; then
        flamepy="$(ls "$local_package"/wheels/*.whl)"
    else
        flamepy="flamepy==$(pep440 "$tag")"
    fi
    log "Installing $flamepy..."
    install_flamepy "$flamepy"

    render_flmenv "$(flmenv_template)" "$prefix/sbin/flmenv.sh" "$prefix" "$installed"
    if [ -n "$conf" ]; then
        {
            echo "# Flame client configuration, generated by sdk.sh from FLAME_* settings"
            echo "---"
            echo "current-context: flame"
            echo "contexts:"
            echo "  - name: flame"
            echo "    cluster:"
            echo "      endpoint: \"$(setting FLAME_ENDPOINT "")\""
            if [ -n "$(setting FLAME_CACHE_ENDPOINT "")" ]; then
                echo "    cache:"
                echo "      endpoint: \"$(setting FLAME_CACHE_ENDPOINT "")\""
            fi
        } > "$conf"
        log "  wrote $conf"
    fi

    log ""
    log "The Flame client is installed at $prefix."
    log "Put it on your environment:  source $prefix/sbin/flmenv.sh"
    log ""
    if [ -n "$conf" ]; then
        log "Client configuration: $conf (from FLAME_ENDPOINT; flmctl reads ~/.flame/flame.yaml)."
    else
        log "No client configuration was written (FLAME_ENDPOINT is not set). Tell the client where the"
        log "cluster is: set FLAME_ENDPOINT (and FLAME_CACHE_ENDPOINT) in the environment or in ./.env and"
        log "run sdk.sh again, or create ~/.flame/flame.yaml:"
        log "  current-context: flame"
        log "  contexts:"
        log "    - name: flame"
        log "      cluster:"
        log "        endpoint: \"http://<head>:8080\""
        log "      cache:"
        log "        endpoint: \"grpc://<worker>:9090\""
        log "Until you configure one, the tools and flamepy report an error."
    fi
}

main
