#!/usr/bin/env bash

# Host-side setup and lifecycle helpers for the CRI Shim E2E job. Test commands
# intentionally remain in the workflow so the CI contract stays visible.

set -Eeuo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

INSTALL_PREFIX="${INSTALL_PREFIX:-/opt/flame-test}"
FLAME_E2E_RUNTIME_IMAGE="${FLAME_E2E_RUNTIME_IMAGE:-localhost:5000/xflops/flmrt:ci}"
FLAME_CRI_SANDBOX_IMAGE="${FLAME_CRI_SANDBOX_IMAGE:-registry.k8s.io/pause:3.10.2}"
FLAME_CLUSTER_CONFIG="${FLAME_CLUSTER_CONFIG:-ci/cri/flame-cluster.yaml}"
CI_ENV_FILE="${GITHUB_ENV:-/tmp/flame-cri-e2e.env}"

install_dependencies() {
    curl -LsSf https://astral.sh/uv/install.sh | sh
    sudo install -m 0755 "$HOME/.local/bin/uv" /usr/bin/uv
    sudo apt-get update
    sudo apt-get install -y \
        apt-transport-https \
        ca-certificates \
        containernetworking-plugins \
        curl \
        gnupg \
        protobuf-compiler
    if ! command -v containerd >/dev/null; then
        sudo apt-get install -y containerd
    fi
    curl -fsSL https://gvisor.dev/archive.key \
        | sudo gpg --dearmor --yes -o /usr/share/keyrings/gvisor-archive-keyring.gpg
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg] https://storage.googleapis.com/gvisor/releases release main" \
        | sudo tee /etc/apt/sources.list.d/gvisor.list >/dev/null
    sudo apt-get update
    sudo apt-get install -y runsc
    runsc --version
}

install_flame() {
    cargo build --release
    sudo ./target/release/flmadm install \
        --all \
        --src-dir . \
        --skip-build \
        --prefix "$INSTALL_PREFIX" \
        --python-version 3.12 \
        --force
    sudo install -m 0644 "$FLAME_CLUSTER_CONFIG" \
        "$INSTALL_PREFIX/conf/flame-cluster.yaml"
    sudo install -d "$INSTALL_PREFIX/conf/applications"
    sudo install -m 0644 ci/cri/applications/*.yaml \
        "$INSTALL_PREFIX/conf/applications/"
    mkdir -p "$HOME/.flame"
    install -m 0644 ci/cri/flame.yaml "$HOME/.flame/flame.yaml"

    set +u
    # shellcheck source=/dev/null
    source "$INSTALL_PREFIX/sbin/flmenv.sh"
    set -u
    {
        echo "PATH=$INSTALL_PREFIX/bin:$PATH"
        echo "PYTHONPATH=${PYTHONPATH:-}"
        echo "LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}"
        echo "FLAME_HOME=$FLAME_HOME"
    } >> "$CI_ENV_FILE"
    python3 -m pip install pytest pytest-timeout
}

publish_runtime_image() {
    docker run -d --name flame-cri-registry -p 5000:5000 registry:2
    docker build -f docker/Dockerfile.flmrt \
        -t "$FLAME_E2E_RUNTIME_IMAGE" .
    docker push "$FLAME_E2E_RUNTIME_IMAGE"
}

cache_runtime_image() {
    sudo ctr --namespace k8s.io images pull --plain-http \
        "$FLAME_E2E_RUNTIME_IMAGE"
    sudo ctr --namespace k8s.io images pull "$FLAME_CRI_SANDBOX_IMAGE"
    sudo ctr --namespace k8s.io images list \
        | grep -F "$FLAME_E2E_RUNTIME_IMAGE"
    sudo ctr --namespace k8s.io images list \
        | grep -F "$FLAME_CRI_SANDBOX_IMAGE"
}

configure_runtime() {
    sudo systemctl stop containerd
    sudo install -d /etc/containerd /etc/containerd/certs.d/localhost:5000
    containerd config default | sudo tee /etc/containerd/config.toml >/dev/null
    if ! grep -q 'io.containerd.cri.v1.images' /etc/containerd/config.toml \
        || ! grep -q 'io.containerd.cri.v1.runtime' /etc/containerd/config.toml; then
        echo 'containerd 2.x CRI plugins are required' >&2
        exit 1
    fi

    sudo sed -i -E \
        "s/(default_runtime_name[[:space:]]*=[[:space:]]*)['\"]runc['\"]/\1'runsc'/" \
        /etc/containerd/config.toml
    sudo sed -i -E \
        "s#(config_path[[:space:]]*=[[:space:]]*)['\"][^'\"]*['\"]#\1'/etc/containerd/certs.d'#" \
        /etc/containerd/config.toml
    sudo sed -i -E \
        "s#(sandbox[[:space:]]*=[[:space:]]*)['\"][^'\"]*['\"]#\1'$FLAME_CRI_SANDBOX_IMAGE'#" \
        /etc/containerd/config.toml
    grep -Eq "sandbox[[:space:]]*=[[:space:]]*['\"]$FLAME_CRI_SANDBOX_IMAGE['\"]" \
        /etc/containerd/config.toml
    sudo tee -a /etc/containerd/config.toml >/dev/null <<'EOF'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runsc]
  runtime_type = 'io.containerd.runsc.v1'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runsc.options]
  TypeUrl = 'io.containerd.runsc.v1.options'
  ConfigPath = '/etc/containerd/runsc.toml'
EOF
    grep -Eq "default_runtime_name[[:space:]]*=[[:space:]]*['\"]runsc['\"]" \
        /etc/containerd/config.toml
    command -v containerd-shim-runsc-v1
    sudo tee /etc/containerd/runsc.toml >/dev/null <<'EOF'
[runsc_config]
  host-uds = 'create'
  platform = 'systrap'
EOF
    sudo tee /etc/containerd/certs.d/localhost:5000/hosts.toml >/dev/null <<'EOF'
server = "http://localhost:5000"

[host."http://localhost:5000"]
  capabilities = ["pull", "resolve"]
EOF
    sudo containerd --config /etc/containerd/config.toml config dump >/dev/null

    sudo install -d /opt/cni/bin /etc/cni/net.d
    for plugin in /usr/lib/cni/*; do
        sudo ln -sf "$plugin" "/opt/cni/bin/$(basename "$plugin")"
    done
    sudo tee /etc/cni/net.d/00-flame.conflist >/dev/null <<'EOF'
{
  "cniVersion": "1.0.0",
  "name": "flame",
  "plugins": [
    {
      "type": "bridge",
      "bridge": "flame0",
      "isGateway": true,
      "ipMasq": true,
      "ipam": {
        "type": "host-local",
        "ranges": [[{"subnet": "10.250.0.0/16"}]],
        "routes": [{"dst": "0.0.0.0/0"}]
      }
    }
  ]
}
EOF

    sudo ip link add flame0 type bridge
    sudo ip address add 10.250.0.1/16 dev flame0
    sudo ip link set flame0 up
    sudo sysctl -w net.ipv4.ip_forward=1
    sudo systemctl restart containerd
    sudo chown "$(id -un):$(id -gn)" /run/containerd/containerd.sock
    test -S /run/containerd/containerd.sock
    sudo ctr plugins ls
    FLAME_CRI_DEDICATED_NODE=true hack/validate-cri-runtime.sh
}

start_cluster() {
    sudo systemctl daemon-reload
    sudo systemctl start flame-object-cache
    sudo systemctl start flame-session-manager
    sudo systemctl start flame-executor-manager

    local ready=false
    for _ in $(seq 1 60); do
        if flmctl list -a > /tmp/flame-applications \
            && grep -q flmrun /tmp/flame-applications \
            && grep -q flmexec /tmp/flame-applications \
            && grep -q flmping /tmp/flame-applications \
            && flmctl list -n > /tmp/flame-nodes \
            && grep -q Ready /tmp/flame-nodes; then
            ready=true
            break
        fi
        sleep 1
    done
    test "$ready" = true
    cat /tmp/flame-applications
    cat /tmp/flame-nodes
}

verify_workloads() {
    : > /tmp/flame-cri-containers
    local container
    for container in $(sudo ctr --namespace k8s.io containers list -q); do
        sudo ctr --namespace k8s.io containers info "$container" \
            | tee -a /tmp/flame-cri-containers
    done
    grep -Eq '"io.xflops.flame.managed-by"[[:space:]]*:[[:space:]]*"executor-manager"' \
        /tmp/flame-cri-containers
    grep -q '"io.xflops.flame.executor-id"' /tmp/flame-cri-containers
}

diagnostics() {
    sudo systemctl status flame-session-manager flame-executor-manager flame-object-cache --no-pager || true
    sudo journalctl -u flame-session-manager -u flame-executor-manager -u flame-object-cache --no-pager -n 1000 || true
    sudo find "$INSTALL_PREFIX/logs" -type f -maxdepth 3 -print -exec tail -n 300 {} \; || true
    sudo find /var/log/flame/executors -type f -print -exec tail -n 300 {} \; || true
    sudo ctr plugins ls || true
    sudo ctr --namespace k8s.io images list || true
    sudo ctr --namespace k8s.io containers list || true
    sudo ctr --namespace k8s.io tasks list || true
    sudo journalctl -u containerd --no-pager -n 500 || true
    docker logs flame-cri-registry || true
    ip address show || true
    ip route show || true
}

stop_services() {
    sudo systemctl stop flame-executor-manager || true
    sudo systemctl stop flame-object-cache || true
    sudo systemctl stop flame-session-manager || true
    sudo systemctl stop containerd || true
    docker stop flame-cri-registry || true
}

uninstall_flame() {
    sudo ./target/release/flmadm uninstall \
        --prefix "$INSTALL_PREFIX" \
        --no-backup \
        --force || true
}

usage() {
    echo "Usage: $0 {install-dependencies|install-flame|publish-runtime-image|configure-runtime|cache-runtime-image|start-cluster|verify-workloads|diagnostics|stop-services|uninstall-flame}" >&2
}

case "${1:-}" in
    install-dependencies) install_dependencies ;;
    install-flame) install_flame ;;
    publish-runtime-image) publish_runtime_image ;;
    configure-runtime) configure_runtime ;;
    cache-runtime-image) cache_runtime_image ;;
    start-cluster) start_cluster ;;
    verify-workloads) verify_workloads ;;
    diagnostics) diagnostics ;;
    stop-services) stop_services ;;
    uninstall-flame) uninstall_flame ;;
    *)
        usage
        exit 2
        ;;
esac
