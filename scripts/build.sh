#!/bin/sh
# Build the Flame release package from this source tree.
#
#   scripts/build.sh [--output DIR] [--tag TAG] [--force]
#
# Builds everything (cargo build --release, the Python SDK wheel, a copy of uv) and writes to DIR:
#
#   package/                                   the package; its sbin/setup.sh and sbin/sdk.sh install it
#   flame-<tag>-<os>-<arch>.tar.gz             that package as one archive
#   <tool>-<tag>-<os>-<arch>                   flmctl, flmping, flmexec and flmbench, one file each
#   a .sha256 file next to each archive and tool
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
FLAME_LIB_DIR="$here/lib"
. "$FLAME_LIB_DIR/common.sh"

usage() {
    cat <<'USAGE'
usage: scripts/build.sh [--output DIR] [--tag TAG] [--force]

  --output DIR  where the assets are written (default ./dist); a non-empty DIR needs --force
  --tag TAG     the version recorded in the package and used in the names
                (default: v<version in common/Cargo.toml>)
  --force       replace a non-empty output directory
USAGE
}

output=./dist
tag=""
force=0
while [ $# -gt 0 ]; do
    case "$1" in
        --output) [ $# -ge 2 ] || die "--output needs a directory"; output="$2"; shift 2 ;;
        --tag) [ $# -ge 2 ] || die "--tag needs a value"; tag="$2"; shift 2 ;;
        --force) force=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

root="${FLAME_SOURCE_DIR:-$(cd "$here/.." && pwd)}"
need cargo
need uv
need tar
detect_platform

if [ -z "$tag" ]; then
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/common/Cargo.toml" | head -n 1)"
    [ -n "$version" ] || die "could not read the version from $root/common/Cargo.toml"
    tag="v$version"
fi
suffix="$tag-$FLAME_OS-$FLAME_ARCH"
name="flame-$suffix"

case "$output" in /*) ;; *) output="$(pwd)/$output" ;; esac
if [ -e "$output" ] && [ -n "$(ls -A "$output" 2>/dev/null)" ] && [ "$force" -ne 1 ]; then
    die "$output is not empty; use --force to replace it"
fi

# The server binaries go to bin/ in the package; the client tools are also published one by one.
binaries="flame-session-manager flame-executor-manager flame-object-cache flmctl flmping flmping-service flmexec flmexec-service flmbench flmbox"
tools="flmctl flmping flmexec flmbench"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT INT TERM
pkg="$stage/arch/$name"
assets="$stage/assets"
mkdir -p "$pkg/bin" "$pkg/sbin" "$pkg/wheels" "$pkg/config/migrations" "$assets"

log "Building Flame..."
(cd "$root" && cargo build --release)
target="$root/target/release"

for binary in $binaries; do
    [ -x "$target/$binary" ] || die "missing binary: $target/$binary"
    install -m 0755 "$target/$binary" "$pkg/bin/$binary"
done
install -m 0755 "$(command -v uv)" "$pkg/bin/uv"

log "Building the Python SDK wheel..."
(cd "$root" && uv build --wheel --out-dir "$pkg/wheels" sdk/python)
rm -f "$pkg/wheels/.gitignore"
[ "$(find "$pkg/wheels" -name '*.whl' | wc -l | tr -d ' ')" -eq 1 ] \
    || die "expected exactly one wheel in $pkg/wheels"

cp -R "$root/common/migrations/sqlite" "$pkg/config/migrations/sqlite"
cp -R "$root/config/applications" "$pkg/config/applications"
cp "$here/flmenv.sh.in" "$pkg/sbin/flmenv.sh.in"
# setup.sh and sdk.sh, self-contained, so a downloaded package installs itself
sh "$here/inline.sh" --version "$tag" "$pkg/sbin"
cp "$root/LICENSE" "$pkg/LICENSE"
printf '%s\n' "$tag" > "$pkg/VERSION"
printf '%s\n' "$FLAME_PACKAGE_FORMAT" > "$pkg/PACKAGE_FORMAT"

log "Archiving $name..."
if tar --version 2>/dev/null | grep -q GNU; then
    tar --owner=0 --group=0 --numeric-owner -czf "$assets/$name.tar.gz" -C "$stage/arch" "$name"
else
    COPYFILE_DISABLE=1 tar -czf "$assets/$name.tar.gz" -C "$stage/arch" "$name"
fi
for tool in $tools; do
    install -m 0755 "$target/$tool" "$assets/$tool-$suffix"
done

for file in "$assets"/*; do
    printf '%s  %s\n' "$(sha256_of "$file")" "$(basename "$file")" > "$file.sha256"
done
mv "$pkg" "$assets/package"

# Only a complete build is written to the output directory.
mkdir -p "$(dirname "$output")"
rm -rf "$output"
mv "$assets" "$output"
log "Done: $output"
log "  install it with: scripts/setup.sh --package $output/package"
