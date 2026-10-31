#!/bin/sh
# Write the hosted setup.sh and sdk.sh: each script with scripts/lib/common.sh (and, inside it,
# scripts/flmenv.sh.in) inlined, so it runs on its own when piped from a URL.
#
#   scripts/inline.sh [--version TAG] OUTPUT_DIR
#
# With --version, TAG is the release the scripts install by default (a release publishes its own scripts);
# without it they install the latest release.
set -eu

tag=""
if [ "${1:-}" = "--version" ]; then
    [ $# -ge 3 ] || { echo "usage: $0 [--version TAG] OUTPUT_DIR" >&2; exit 2; }
    tag="$2"
    shift 2
fi
[ $# -eq 1 ] || { echo "usage: $0 [--version TAG] OUTPUT_DIR" >&2; exit 2; }
here="$(cd "$(dirname "$0")" && pwd)"
out="$1"
mkdir -p "$out"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

awk -v template="$here/flmenv.sh.in" -v tag="$tag" '
    /^#@include flmenv\.sh\.in$/ { while ((getline line < template) > 0) print line; close(template); next }
    /^FLAME_DEFAULT_VERSION=""$/ && tag != "" { print "FLAME_DEFAULT_VERSION=\"" tag "\""; next }
    { print }
' "$here/lib/common.sh" > "$work/common.sh"

for script in setup.sh sdk.sh; do
    awk -v lib="$work/common.sh" '
        /#@include lib\/common\.sh$/ { while ((getline line < lib) > 0) print line; close(lib); next }
        { print }
    ' "$here/$script" > "$out/$script"
    chmod 0755 "$out/$script"
    if grep -q '#@include' "$out/$script"; then
        echo "error: $script still has an unresolved #@include" >&2
        exit 1
    fi
    sh -n "$out/$script"
done
