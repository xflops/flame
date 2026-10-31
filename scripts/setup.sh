#!/bin/sh
# Install a Flame cluster node from a release package: the head node, or a worker that joins it.
#
#   curl -fsSL https://xflops.io/flame/setup.sh | sh                              # the head node
#   curl -fsSL https://xflops.io/flame/setup.sh | sh -s -- --join                 # a worker
#
# See docs/designs/RFE683-script-install/FS.md. POSIX sh; run as root for the default prefix.
set -eu

FLAME_LIB_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/lib" || FLAME_LIB_DIR=/nonexistent
. "$FLAME_LIB_DIR/common.sh" #@include lib/common.sh

usage() {
    cat <<'USAGE'
usage: setup.sh [--join] [--prefix DIR] [--package DIR|FILE|URL] [--version TAG]
                [--python-version V]... [--no-systemd] [--force]

  (no --join)          install the head node: the session manager
  --join               install a worker: the executor manager and the object cache
  --prefix DIR         installation directory (default /usr/local/flame)
  --package X          a package directory, a .tar.gz archive, or the http(s) URL of an archive
                       (default: the package this script is in, else the latest release, or
                       --version / FLAME_VERSION)
  --python-version V   Python version to install flamepy for (repeatable; default 3.11 3.12 3.13 3.14)
  --no-systemd         do not write unit files or start anything
  --force              replace an installation that is already there without asking (without it,
                       setup.sh asks, and with no terminal to ask on, stops)

environment: FLAME_VERSION, FLAME_BASE_URL (release download base, for a mirror)

setup.sh writes conf/flame-cluster.yaml from FLAME_* settings, in the environment or in ./.env (the
environment wins), with these defaults: FLAME_CLUSTER_NAME=flame, FLAME_ENDPOINT=http://127.0.0.1:8080,
FLAME_RESREQ=cpu=1,mem=2g, FLAME_POLICIES="priority drf das", FLAME_STORAGE=fs://PREFIX/data,
FLAME_SHIMS=host, FLAME_MAX_EXECUTORS=128, FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090,
FLAME_CACHE_ADVERTISE=lo, FLAME_CACHE_STORAGE=PREFIX/data/cache. Edit the file for anything else.
USAGE
}

SERVICE_SESSION_MANAGER=flame-session-manager
SERVICE_EXECUTOR_MANAGER=flame-executor-manager
SERVICE_OBJECT_CACHE=flame-object-cache
SYSTEMD_DIR="${FLAME_SYSTEMD_DIR:-/etc/systemd/system}"
SYSTEMCTL="${FLAME_SYSTEMCTL:-systemctl}"

join=0
prefix=/usr/local/flame
package=""
version=""
py_versions=""
systemd=1
force=0

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --join) join=1; shift ;;
            --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix="$2"; shift 2 ;;
            --package) [ $# -ge 2 ] || die "--package needs a directory, archive or URL"; package="$2"; shift 2 ;;
            --version) [ $# -ge 2 ] || die "--version needs a tag"; version="$2"; shift 2 ;;
            --python-version) [ $# -ge 2 ] || die "--python-version needs a version"; py_versions="$py_versions $2"; shift 2 ;;
            --no-systemd) systemd=0; shift ;;
            --force) force=1; shift ;;
            -h | --help) usage; exit 0 ;;
            *) usage >&2; exit 2 ;;
        esac
    done
    [ -n "$py_versions" ] || py_versions="3.11 3.12 3.13 3.14"
    case "$prefix" in /*) ;; *) die "--prefix must be an absolute path" ;; esac
}

# --- the package -------------------------------------------------------------------------------

extract_archive() {
    mkdir -p "$work/x"
    tar -xzf "$1" -C "$work/x" || die "failed to extract $1"
    package_in "$work/x" || die "$1 does not hold a release package (one directory with bin/, wheels/ and PACKAGE_FORMAT)"
}

# acquire_package sets $pkg to a package directory.
acquire_package() {
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT INT TERM
    case "$package" in
        http://* | https://* | file://*)
            log "Downloading $package..."
            fetch_verified "$package" "$work/package.tar.gz"
            pkg="$(extract_archive "$work/package.tar.gz")"
            ;;
        "")
            if own="$(own_package)" && [ -z "$version" ] && [ -z "${FLAME_VERSION:-}" ]; then
                # run as <package>/sbin/setup.sh: install the package it came in
                pkg="$own"
            else
                detect_platform
                tag="$(resolve_tag "$version")"
                base="$(release_base "$tag")"
                archive="flame-$tag-$FLAME_OS-$FLAME_ARCH.tar.gz"
                log "Downloading Flame $tag ($FLAME_OS-$FLAME_ARCH)..."
                fetch_verified "$base/$archive" "$work/package.tar.gz"
                pkg="$(extract_archive "$work/package.tar.gz")"
            fi
            ;;
        *)
            if [ -d "$package" ]; then
                pkg="$(package_in "$package")" \
                    || die "$package is not a release package (expected PACKAGE_FORMAT, bin/ and wheels/, here or in its one subdirectory)"
            elif [ -f "$package" ]; then
                # A checksum next to the archive is verified; a bare archive is trusted.
                if [ -f "$package.sha256" ]; then
                    verify_checksum "$package" "$package.sha256"
                fi
                pkg="$(extract_archive "$package")"
            else
                die "--package $package: not a directory, an archive, or an http(s) URL"
            fi
            ;;
    esac
    check_package_format "$pkg"
    log "Using release package: $pkg ($(cat "$pkg/VERSION" 2>/dev/null || echo unknown version))"
}

# --- installing --------------------------------------------------------------------------------

# target_files lists the files this install writes, so an existing one is found before anything changes.
target_files() {
    printf '%s\n' "$prefix/conf/flame-cluster.yaml"
    if [ "$join" -eq 0 ]; then
        printf '%s\n' "$prefix/bin/flame-session-manager"
        for manifest in "$pkg"/config/applications/*.yaml "$pkg"/config/applications/*.yml; do
            [ -f "$manifest" ] && printf '%s\n' "$prefix/conf/applications/$(basename "$manifest")"
        done
        for migration in "$pkg"/config/migrations/sqlite/*; do
            [ -f "$migration" ] && printf '%s\n' "$prefix/migrations/sqlite/$(basename "$migration")"
        done
        [ "$systemd" -eq 1 ] && printf '%s\n' "$(unit_file "$SERVICE_SESSION_MANAGER")"
    else
        for binary in flame-executor-manager flame-object-cache flmping-service flmexec-service flmbox uv; do
            printf '%s\n' "$prefix/bin/$binary"
        done
        printf '%s\n' "$prefix/sbin/flmenv.sh"
        if [ "$systemd" -eq 1 ]; then
            printf '%s\n' "$(unit_file "$SERVICE_OBJECT_CACHE")" "$(unit_file "$SERVICE_EXECUTOR_MANAGER")"
        fi
    fi
}

# check_existing: when something this install would write already exists, ask before overwriting
# (--force answers yes); with no terminal to ask on, or a no, stop with nothing written.
check_existing() {
    [ "$force" -ne 1 ] || return 0
    target_files | while IFS= read -r file; do
        if [ -e "$file" ]; then
            echo "$file"
            break
        fi
    done > "$work/existing"
    existing="$(cat "$work/existing")"
    [ -n "$existing" ] || return 0
    log "$existing already exists: there is a Flame installation at $prefix."
    status=0
    confirm "Overwrite the existing installation?" || status=$?
    case "$status" in
        0) ;;
        1) die "not overwriting $existing; nothing was changed" ;;
        *) die "$existing already exists and there is no terminal to ask on; use --force to replace the installation" ;;
    esac
}

make_dirs() {
    mkdir -p "$prefix" 2>/dev/null || die "cannot create $prefix: run as root (sudo) or pass --prefix"
    for dir in bin sbin lib conf logs work wheels migrations/sqlite data/cache; do
        mkdir -p "$prefix/$dir"
    done
    chmod 0700 "$prefix/data"
}

install_bin() {
    [ -f "$pkg/bin/$1" ] || die "the package has no bin/$1"
    install -m 0755 "$pkg/bin/$1" "$prefix/bin/$1"
    log "  installed $1"
}

# install_python_sdk installs flamepy for each requested Python and sets $installed_versions.
install_python_sdk() {
    uv="$prefix/bin/uv"
    rm -rf "$prefix/wheels"
    mkdir -p "$prefix/wheels"
    cp "$pkg"/wheels/*.whl "$prefix/wheels/"
    wheel="$(ls "$prefix"/wheels/*.whl)"
    installed_versions=""
    for v in $py_versions; do
        log "  installing flamepy for Python $v..."
        if output="$(UV_CACHE_DIR="$prefix/data/cache/uv" "$uv" pip install --python "python$v" --prefix "$prefix" "$wheel" 2>&1)"; then
            installed_versions="$installed_versions $v"
        else
            case "$output" in
                *"No interpreter found"* | *"not found"* | *"Cannot find"*)
                    warn "Python $v is not available, skipping" ;;
                *) die "failed to install flamepy for Python $v: $output" ;;
            esac
        fi
    done
    installed_versions="${installed_versions# }"
    [ -n "$installed_versions" ] || die "failed to install flamepy: no Python interpreter found for $py_versions"
}

# default_python_version: the host's python3 when flamepy is installed for it, else the newest installed.
default_python_version() {
    host="$(python_version_of python3)"
    for v in $installed_versions; do
        if [ "$v" = "$host" ]; then
            printf '%s' "$host"
            return 0
        fi
    done
    newest_version "$installed_versions"
}

render_env() {
    template="$pkg/sbin/flmenv.sh.in"
    [ -f "$template" ] || template="$(flmenv_template)"
    render_flmenv "$template" "$prefix/sbin/flmenv.sh" "$prefix" "$installed_versions"
    log "  generated $prefix/sbin/flmenv.sh"
}

# write_config generates conf/flame-cluster.yaml from the FLAME_* settings, with defaults for what is unset.
write_config() {
    conf="$prefix/conf/flame-cluster.yaml"
    policies=""
    for policy in $(setting FLAME_POLICIES "priority drf das" | tr ',' ' '); do
        policies="$policies    - $policy
"
    done
    shims="$(setting FLAME_SHIMS host | tr ' ' ',' | sed 's/,,*/, /g')"
    mkdir -p "$prefix/conf"
    cat > "$conf" <<YAML
# Flame cluster configuration, generated by setup.sh from FLAME_* settings
---
cluster:
  name: "$(setting FLAME_CLUSTER_NAME flame)"
  endpoint: "$(setting FLAME_ENDPOINT http://127.0.0.1:8080)"
  resreq: "$(setting FLAME_RESREQ cpu=1,mem=2g)"
  policies:
${policies}  storage: "$(setting FLAME_STORAGE "fs://$prefix/data")"
  executors:
    shims: [$shims]
  limits:
    node:
      max_executors: $(setting FLAME_MAX_EXECUTORS 128)
cache:
  endpoint: "$(setting FLAME_CACHE_ENDPOINT grpc://127.0.0.1:9090)"
  advertise: "$(setting FLAME_CACHE_ADVERTISE lo)"
  storage: "$(setting FLAME_CACHE_STORAGE "$prefix/data/cache")"
YAML
    log "  wrote $conf"
}

install_manifests_and_migrations() {
    mkdir -p "$prefix/conf/applications"
    for manifest in "$pkg"/config/applications/*.yaml "$pkg"/config/applications/*.yml; do
        [ -f "$manifest" ] || continue
        cp "$manifest" "$prefix/conf/applications/$(basename "$manifest")"
    done
    # the session manager reads migrations/sqlite relative to its working directory, the prefix
    cp "$pkg"/config/migrations/sqlite/* "$prefix/migrations/sqlite/"
}

# --- systemd -----------------------------------------------------------------------------------

unit_file() { printf '%s/%s.service' "$SYSTEMD_DIR" "$1"; }

write_unit() { # name description requires extra-env binary log
    {
        cat <<UNIT
[Unit]
Description=$2
Documentation=https://github.com/xflops-io/flame
After=network.target$3
Wants=network-online.target
UNIT
        [ -z "$3" ] || printf 'Requires=%s\n' "${3# }"
        cat <<UNIT

[Service]
Type=simple
Environment="RUST_LOG=info"
Environment="FLAME_HOME=$prefix"
UNIT
        [ -z "$4" ] || printf 'Environment="%s"\n' "$4"
        cat <<UNIT
WorkingDirectory=$prefix
ExecStart=$prefix/bin/$5 --config $prefix/conf/flame-cluster.yaml
StandardOutput=append:$prefix/logs/$6.log
StandardError=append:$prefix/logs/$6.log
Restart=on-failure
RestartSec=5s
LimitNOFILE=65536
UNIT
        [ "$5" != flame-object-cache ] || echo "MemoryMax=12G"
        cat <<UNIT

[Install]
WantedBy=multi-user.target
UNIT
    } > "$(unit_file "$1")"
}

write_units() {
    mkdir -p "$SYSTEMD_DIR"
    if [ "$join" -eq 0 ]; then
        write_unit "$SERVICE_SESSION_MANAGER" "Flame Session Manager" "" "" flame-session-manager fsm
        units="$SERVICE_SESSION_MANAGER"
    else
        write_unit "$SERVICE_OBJECT_CACHE" "Flame Object Cache" "" "" flame-object-cache foc
        write_unit "$SERVICE_EXECUTOR_MANAGER" "Flame Executor Manager" "" "FLAME_PYTHON_VERSION=$(default_python_version)" flame-executor-manager fem
        units="$SERVICE_OBJECT_CACHE $SERVICE_EXECUTOR_MANAGER"
    fi
    log "  wrote systemd units: $units"
}

enable_units() {
    "$SYSTEMCTL" daemon-reload
    for unit in $units; do
        "$SYSTEMCTL" enable "$unit"
        # restart, not start: a rerun replaced the binaries of a running service
        "$SYSTEMCTL" restart "$unit"
        tries=0
        until "$SYSTEMCTL" is-active "$unit" >/dev/null 2>&1; do
            tries=$((tries + 1))
            [ "$tries" -le 15 ] || die "$unit did not become active; see journalctl -u $unit"
            sleep 1
        done
        log "  $unit is running"
    done
}

# --- installing: the flow ----------------------------------------------------------------------

install_node() {
    check_settings FLAME_CLUSTER_NAME FLAME_ENDPOINT FLAME_RESREQ FLAME_POLICIES FLAME_STORAGE FLAME_SHIMS \
        FLAME_MAX_EXECUTORS FLAME_CACHE_ENDPOINT FLAME_CACHE_ADVERTISE FLAME_CACHE_STORAGE
    acquire_package
    check_existing
    log "Installing to $prefix..."
    make_dirs
    write_config
    if [ "$join" -eq 0 ]; then
        install_bin flame-session-manager
        install_manifests_and_migrations
        # the head runs no Python application, but flmenv.sh and the unit need a version to name
        installed_versions=""
    else
        for binary in flame-executor-manager flame-object-cache flmping-service flmexec-service flmbox uv; do
            install_bin "$binary"
        done
        install_python_sdk
        render_env
    fi
    if [ "$systemd" -eq 1 ]; then
        if [ -z "${FLAME_SYSTEMD_DIR:-}" ]; then
            have "$SYSTEMCTL" || die "$SYSTEMCTL not found; use --no-systemd to install without services"
        fi
        write_units
        enable_units
    else
        log "Skipped systemd (--no-systemd). Run the binaries with:"
        if [ "$join" -eq 0 ]; then
            log "  FLAME_HOME=$prefix $prefix/bin/flame-session-manager --config $prefix/conf/flame-cluster.yaml"
        else
            log "  FLAME_HOME=$prefix $prefix/bin/flame-object-cache --config $prefix/conf/flame-cluster.yaml"
            log "  FLAME_HOME=$prefix $prefix/bin/flame-executor-manager --config $prefix/conf/flame-cluster.yaml"
        fi
    fi
    summary
}

summary() {
    log ""
    log "Flame is installed at $prefix."
    log "Cluster configuration: $prefix/conf/flame-cluster.yaml (generated from FLAME_* settings; edit it for more)."
    if [ "$join" -eq 0 ]; then
        log "Join workers with:"
        log "  curl -fsSL https://xflops.io/flame/setup.sh | sudo sh -s -- --join"
        log "Install a client with:"
        log "  curl -fsSL https://xflops.io/flame/sdk.sh | sh"
    else
        log "Environment: source $prefix/sbin/flmenv.sh"
    fi
}

main() {
    load_dotenv ./.env
    parse_args "$@"
    install_node
}

main "$@"
