#!/usr/bin/env bash

# Validate the site-managed CRI runtime prerequisites required by Flame.
# This script is read-only and is suitable for both worker preflight and CI.

set -Eeuo pipefail

CONTAINERD_CONFIG="${CONTAINERD_CONFIG:-/etc/containerd/config.toml}"
RUNSC_CONFIG="${RUNSC_CONFIG:-/etc/containerd/runsc.toml}"
CNI_CONFIG="${CNI_CONFIG:-/etc/cni/net.d/00-flame.conflist}"
FLAME_CRI_PLATFORM="${FLAME_CRI_PLATFORM:-systrap}"
FLAME_CRI_DEDICATED_NODE="${FLAME_CRI_DEDICATED_NODE:-false}"
CONTAINERD_SOCKET="${CONTAINERD_SOCKET:-/run/containerd/containerd.sock}"

fail() {
    echo "CRI runtime preflight failed: $*" >&2
    exit 1
}

case "$FLAME_CRI_PLATFORM" in
    systrap | kvm) ;;
    *) fail "unsupported gVisor platform <$FLAME_CRI_PLATFORM>" ;;
esac
case "$FLAME_CRI_DEDICATED_NODE" in
    true | false) ;;
    *) fail "FLAME_CRI_DEDICATED_NODE must be <true> or <false>" ;;
esac

require_command() {
    command -v "$1" >/dev/null || fail "command <$1> is unavailable"
}

require_file() {
    test -r "$1" || fail "configuration <$1> is not readable"
}

require_command containerd
require_command containerd-shim-runsc-v1
require_command runsc
require_file "$CONTAINERD_CONFIG"
require_file "$RUNSC_CONFIG"
require_file "$CNI_CONFIG"
test -S "$CONTAINERD_SOCKET" || fail "CRI socket <$CONTAINERD_SOCKET> is unavailable"
test -w "$CONTAINERD_SOCKET" \
    || fail "CRI socket <$CONTAINERD_SOCKET> is not writable by the current user"

effective_config="$(containerd --config "$CONTAINERD_CONFIG" config dump)"
grep -Eq "default_runtime_name[[:space:]]*=[[:space:]]*['\"]runsc['\"]" \
    <<<"$effective_config" \
    || fail "containerd default CRI runtime is not <runsc>"
grep -Eq "runtime_type[[:space:]]*=[[:space:]]*['\"]io\.containerd\.runsc\.v1['\"]" \
    <<<"$effective_config" \
    || fail "containerd runsc runtime type is not <io.containerd.runsc.v1>"
grep -F "$RUNSC_CONFIG" <<<"$effective_config" | grep -q 'ConfigPath' \
    || fail "containerd does not reference runsc configuration <$RUNSC_CONFIG>"

grep -Eq "host-uds[[:space:]]*=[[:space:]]*['\"]create['\"]" "$RUNSC_CONFIG" \
    || fail "runsc must set <host-uds = create>"
grep -Eq "platform[[:space:]]*=[[:space:]]*['\"]$FLAME_CRI_PLATFORM['\"]" \
    "$RUNSC_CONFIG" \
    || fail "runsc platform is not <$FLAME_CRI_PLATFORM>"
if [[ "$FLAME_CRI_PLATFORM" == "kvm" ]]; then
    test -c /dev/kvm || fail "KVM platform requires /dev/kvm"
    test -r /dev/kvm && test -w /dev/kvm \
        || fail "/dev/kvm is not accessible by the current user"
fi

grep -Eq '"type"[[:space:]]*:[[:space:]]*"bridge"' "$CNI_CONFIG" \
    || fail "CNI configuration has no bridge plugin"
grep -Eq '"type"[[:space:]]*:[[:space:]]*"host-local"' "$CNI_CONFIG" \
    || fail "CNI configuration has no host-local IPAM"
if [[ "$FLAME_CRI_DEDICATED_NODE" == "true" ]] \
    && grep -Eq '"type"[[:space:]]*:[[:space:]]*"portmap"' "$CNI_CONFIG"; then
    fail "dedicated Flame worker CNI configuration still enables unused portmap"
fi

containerd --version
runsc --version
echo "CRI runtime preflight passed: platform=$FLAME_CRI_PLATFORM dedicated_node=$FLAME_CRI_DEDICATED_NODE"
