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

  --prefix DIR         where bin/, lib/, wheels/, examples/ and sbin/flmenv.sh go (default $HOME/.flame)
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
        for file in "$prefix/bin/flmctl" "$prefix/bin/flmping" "$prefix/bin/flmexec" "$prefix/bin/flmbench" "$prefix/sbin/flmenv.sh" "$prefix/examples" "$prefix/wheels" "$conf"; do
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

    # src is the package: the one this script is in, or the downloaded and verified package archive of
    # the release (the same archive setup.sh installs)
    if [ -n "$local_package" ]; then
        src="$local_package"
        log "Using the client in $src..."
    else
        name="flame-$suffix"
        log "Downloading Flame $tag..."
        fetch_verified "$base/$name.tar.gz" "$work/$name.tar.gz"
        tar -xzf "$work/$name.tar.gz" -C "$work" || die "cannot unpack $name.tar.gz"
        src="$work/$name"
        [ -f "$src/PACKAGE_FORMAT" ] || die "$name.tar.gz has no $name package directory"
        check_package_format "$src"
    fi
    for tool in flmctl flmping flmexec flmbench uv; do
        [ -f "$src/bin/$tool" ] || die "the client has no bin/$tool"
    done
    [ -d "$src/examples" ] || die "the client has no examples"
    flamepy="$(find "$src/wheels" -name '*.whl' 2>/dev/null | head -n 1)"
    [ -n "$flamepy" ] || die "the client has no flamepy wheel"

    # everything is checked before anything is installed
    # the client tools and uv (which runs the examples)
    for tool in flmctl flmping flmexec flmbench uv; do
        install -m 0755 "$src/bin/$tool" "$prefix/bin/$tool"
        log "  installed $prefix/bin/$tool"
    done
    # the examples go to <prefix>/examples, replacing an earlier copy
    rm -rf "$prefix/examples"
    cp -R "$src/examples" "$prefix/examples"
    log "  installed $prefix/examples"

    # the wheel stays in <prefix>/wheels, as setup.sh keeps it, and is installed from there
    rm -rf "$prefix/wheels"
    mkdir -p "$prefix/wheels"
    cp "$flamepy" "$prefix/wheels/"
    flamepy="$prefix/wheels/$(basename "$flamepy")"
    log "Installing $(basename "$flamepy")..."
    install_flamepy "$flamepy"

    # sbin/flmenv.sh: the package's template, rendered for this prefix and the Pythons flamepy went into
    template="$src/sbin/flmenv.sh.in"
    [ -f "$template" ] || template="$(flmenv_template)"
    render_flmenv "$template" "$prefix/sbin/flmenv.sh" "$prefix" "$installed"
    log "  generated $prefix/sbin/flmenv.sh"
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
