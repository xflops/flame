#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

RELEASE="${RELEASE:-flame}"
NAMESPACE="${NAMESPACE:-flame-k8s-e2e}"
CHART_DIR="${CHART_DIR:-${ROOT_DIR}/charts/flame}"
IMAGE_REGISTRY="${IMAGE_REGISTRY:-xflops}"
IMAGE_TAG="${IMAGE_TAG:-ci}"
IMAGE_PULL_POLICY="${IMAGE_PULL_POLICY:-IfNotPresent}"
OBJECT_CACHE_REPLICAS="${OBJECT_CACHE_REPLICAS:-2}"
SESSION_MANAGER_STORAGE="${SESSION_MANAGER_STORAGE:-fs:///var/lib/flame/session}"
TIMEOUT="${TIMEOUT:-10m}"
FLMPING_TASKS="${FLMPING_TASKS:-3}"
APP_E2E_TASKS="${APP_E2E_TASKS:-3}"
PI_NUM_BATCHES="${PI_NUM_BATCHES:-2}"
PI_SAMPLES_PER_BATCH="${PI_SAMPLES_PER_BATCH:-1000}"
AUTO_DIAGNOSTICS="${AUTO_DIAGNOSTICS:-true}"

CERT_MANAGER_VERSION="${CERT_MANAGER_VERSION:-v1.21.2}"
ENVOY_GATEWAY_VERSION="${ENVOY_GATEWAY_VERSION:-v1.9.1}"
CERT_MANAGER_NAMESPACE="${CERT_MANAGER_NAMESPACE:-cert-manager}"
ENVOY_GATEWAY_NAMESPACE="${ENVOY_GATEWAY_NAMESPACE:-envoy-gateway-system}"
SESSION_NODE_PORT="${SESSION_NODE_PORT:-30080}"
CACHE_GATEWAY_NODE_PORT="${CACHE_GATEWAY_NODE_PORT:-30443}"
CACHE_GATEWAY_HOST="${CACHE_GATEWAY_HOST:-cache.flame.test}"

EXTERNAL_SESSION_SERVICE="${RELEASE}-session-external"
GATEWAY_CLASS="${RELEASE}-cache-e2e"
GATEWAY="${RELEASE}-cache"
ENVOY_PROXY="${RELEASE}-cache-nodeport"
CACHE_BOOTSTRAP_ROUTE="${RELEASE}-cache-bootstrap"
CACHE_OWNER_ROUTE="${RELEASE}-cache-owner"
CACHE_BACKEND="${RELEASE}-cache-owner"
CACHE_SECURITY_POLICY="${RELEASE}-cache-owner"
CACHE_CA_SECRET="${RELEASE}-cache-ca"
CACHE_TLS_SECRET="${RELEASE}-cache-tls"

EXTERNAL_ACCESS_TEMPLATE="${ROOT_DIR}/ci/k8s/external-access.yaml"
VM_CLIENT_CONFIG_TEMPLATE="${ROOT_DIR}/ci/k8s/flame-vm.yaml"
RENDERED_EXTERNAL_ACCESS="${TMPDIR:-/tmp}/flame-k8s-e2e-external-access.yaml"

CLIENT_ROOT=""
CLIENT_CONTAINER=""

HELM_E2E_ARGS=(
    --set "global.imageRegistry=${IMAGE_REGISTRY}"
    --set "global.imageTag=${IMAGE_TAG}"
    --set "global.imagePullPolicy=${IMAGE_PULL_POLICY}"
    --set "cluster.storage=${SESSION_MANAGER_STORAGE}"
    --set "cluster.policies[0]=priority"
    --set "cluster.policies[1]=das"
    --set "cluster.executors.shim=host"
    --set cluster.limits.maxExecutors=10
    --set sessionManager.persistence.enabled=false
    --set objectCache.persistence.enabled=false
    --set "objectCache.replicas=${OBJECT_CACHE_REPLICAS}"
    --set executorManager.replicas=1
)

log() {
    printf '[k8s-e2e] %s\n' "$*"
}

dump_debug() {
    log "Kubernetes resources"
    kubectl -n "$NAMESPACE" get all,pvc 2>/dev/null || true

    log "External access resources"
    kubectl get gatewayclass "$GATEWAY_CLASS" -o yaml 2>/dev/null || true
    kubectl -n "$NAMESPACE" get gateway,httproute,backend,securitypolicy,issuer,certificate -o wide 2>/dev/null || true
    kubectl -n "$NAMESPACE" describe gateway "$GATEWAY" 2>/dev/null || true

    log "cert-manager resources"
    kubectl get issuer,certificate,certificaterequest,order,challenge -A 2>/dev/null || true

    log "Recent events"
    kubectl -n "$NAMESPACE" get events --sort-by=.lastTimestamp 2>/dev/null || true

    for component in session-manager object-cache executor-manager; do
        log "Logs for component=${component}"
        kubectl -n "$NAMESPACE" logs -l "app.kubernetes.io/component=${component}" --all-containers --tail=500 2>/dev/null || true
    done

    log "Envoy Gateway resources"
    kubectl -n "$ENVOY_GATEWAY_NAMESPACE" get all 2>/dev/null || true
    kubectl -n "$ENVOY_GATEWAY_NAMESPACE" logs deployment/envoy-gateway --all-containers --tail=500 2>/dev/null || true
    kubectl -n "$ENVOY_GATEWAY_NAMESPACE" logs \
        -l "gateway.envoyproxy.io/owning-gateway-name=${GATEWAY}" \
        --all-containers --tail=500 2>/dev/null || true
}

cleanup() {
    if [[ -n "$CLIENT_CONTAINER" ]]; then
        docker rm -f "$CLIENT_CONTAINER" >/dev/null 2>&1 || true
    fi
    if [[ -n "$CLIENT_ROOT" && -d "$CLIENT_ROOT" ]]; then
        rm -rf -- "$CLIENT_ROOT"
    fi
    rm -f -- "$RENDERED_EXTERNAL_ACCESS"
}

finish() {
    local rc=$?
    if [[ "$rc" -ne 0 && "$AUTO_DIAGNOSTICS" == "true" ]]; then
        dump_debug
    fi
    cleanup
    exit "$rc"
}

wait_rollout() {
    local kind="$1"
    local component="$2"

    log "Waiting for ${kind}/${component}"
    kubectl -n "$NAMESPACE" rollout status "$kind" \
        -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${component}" \
        --timeout="$TIMEOUT"
}

wait_route_ready() {
    local route="$1"
    local deadline=$((SECONDS + 300))
    local accepted=""
    local resolved_refs=""

    while (( SECONDS < deadline )); do
        accepted="$(kubectl -n "$NAMESPACE" get httproute "$route" \
            -o 'jsonpath={.status.parents[0].conditions[?(@.type=="Accepted")].status}' 2>/dev/null || true)"
        resolved_refs="$(kubectl -n "$NAMESPACE" get httproute "$route" \
            -o 'jsonpath={.status.parents[0].conditions[?(@.type=="ResolvedRefs")].status}' 2>/dev/null || true)"
        if [[ "$accepted" == "True" && "$resolved_refs" == "True" ]]; then
            return 0
        fi
        sleep 2
    done

    log "HTTPRoute/${route} was not accepted with resolved references"
    kubectl -n "$NAMESPACE" get httproute "$route" -o yaml || true
    return 1
}

wait_backend_accepted() {
    local deadline=$((SECONDS + 300))
    local accepted=""

    while (( SECONDS < deadline )); do
        accepted="$(kubectl -n "$NAMESPACE" get backend "$CACHE_BACKEND" \
            -o 'jsonpath={.status.conditions[?(@.type=="Accepted")].status}' 2>/dev/null || true)"
        if [[ "$accepted" == "True" ]]; then
            return 0
        fi
        sleep 2
    done

    log "Backend/${CACHE_BACKEND} was not accepted"
    kubectl -n "$NAMESPACE" get backend "$CACHE_BACKEND" -o yaml || true
    return 1
}

wait_security_policy_accepted() {
    local deadline=$((SECONDS + 300))
    local accepted=""

    while (( SECONDS < deadline )); do
        accepted="$(kubectl -n "$NAMESPACE" get securitypolicy "$CACHE_SECURITY_POLICY" \
            -o 'jsonpath={.status.ancestors[0].conditions[?(@.type=="Accepted")].status}' 2>/dev/null || true)"
        if [[ "$accepted" == "True" ]]; then
            return 0
        fi
        sleep 2
    done

    log "SecurityPolicy/${CACHE_SECURITY_POLICY} was not accepted"
    kubectl -n "$NAMESPACE" get securitypolicy "$CACHE_SECURITY_POLICY" -o yaml || true
    return 1
}

install_external_infrastructure() {
    log "Installing cert-manager ${CERT_MANAGER_VERSION}"
    helm upgrade --install cert-manager oci://quay.io/jetstack/charts/cert-manager \
        --version "$CERT_MANAGER_VERSION" \
        --namespace "$CERT_MANAGER_NAMESPACE" \
        --create-namespace \
        --set crds.enabled=true \
        --wait \
        --timeout "$TIMEOUT"

    log "Installing Envoy Gateway ${ENVOY_GATEWAY_VERSION}"
    helm upgrade --install envoy-gateway oci://docker.io/envoyproxy/gateway-helm \
        --version "$ENVOY_GATEWAY_VERSION" \
        --namespace "$ENVOY_GATEWAY_NAMESPACE" \
        --create-namespace \
        --set config.envoyGateway.extensionApis.enableBackend=true \
        --wait \
        --timeout "$TIMEOUT"
}

install_flame() {
    log "Linting chart"
    helm lint "$CHART_DIR" "${HELM_E2E_ARGS[@]}" "$@"

    log "Rendering chart"
    helm template "$RELEASE" "$CHART_DIR" \
        --namespace "$NAMESPACE" \
        "${HELM_E2E_ARGS[@]}" \
        "$@" >"${TMPDIR:-/tmp}/flame-k8s-e2e-rendered.yaml"

    log "Installing chart into namespace ${NAMESPACE}"
    helm upgrade --install "$RELEASE" "$CHART_DIR" \
        --namespace "$NAMESPACE" \
        --create-namespace \
        --wait \
        --timeout "$TIMEOUT" \
        "${HELM_E2E_ARGS[@]}" \
        "$@"

    wait_rollout deployment session-manager
    wait_rollout statefulset object-cache
    wait_rollout deployment executor-manager
}

create_external_access() {
    local cache_service="$1"
    local session_frontend_port="$2"
    local cache_flight_port="$3"
    local cache_authority_expression=""
    local cache_certificate_ips=""
    local pod_ip=""
    local -a cache_pod_ips=()

    mapfile -t cache_pod_ips < <(kubectl -n "$NAMESPACE" get pod \
        -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=object-cache" \
        -o json | jq -r \
        '.items[] | select(any(.status.conditions[]?; .type == "Ready" and .status == "True")) | .status.podIP')
    if [[ "${#cache_pod_ips[@]}" -ne "$OBJECT_CACHE_REPLICAS" ]]; then
        log "Expected ${OBJECT_CACHE_REPLICAS} ready object-cache pod IPs, found ${#cache_pod_ips[@]}"
        return 1
    fi

    for pod_ip in "${cache_pod_ips[@]}"; do
        if [[ -n "$cache_authority_expression" ]]; then
            cache_authority_expression+=" || "
        fi
        cache_authority_expression+="request.host == '${pod_ip}:${cache_flight_port}'"
        if [[ -n "$cache_certificate_ips" ]]; then
            cache_certificate_ips+=$'\n'
        fi
        cache_certificate_ips+="    - ${pod_ip}"
    done
    : "${cache_authority_expression:?no object-cache pod IPs found}"
    : "${cache_certificate_ips:?no object-cache certificate IPs found}"

    export EXTERNAL_SESSION_SERVICE NAMESPACE RELEASE SESSION_NODE_PORT
    export CACHE_GATEWAY_NODE_PORT CACHE_GATEWAY_HOST GATEWAY_CLASS GATEWAY
    export ENVOY_PROXY CACHE_BOOTSTRAP_ROUTE CACHE_OWNER_ROUTE CACHE_BACKEND
    export CACHE_SECURITY_POLICY CACHE_CA_SECRET CACHE_TLS_SECRET
    export CACHE_SERVICE="$cache_service"
    export SESSION_FRONTEND_PORT="$session_frontend_port"
    export CACHE_FLIGHT_PORT="$cache_flight_port"
    export CACHE_AUTHORITY_EXPRESSION="$cache_authority_expression"
    export CACHE_CERTIFICATE_IPS="$cache_certificate_ips"

    log "Creating CI-owned session NodePort and cache Gateway resources"
    envsubst '${EXTERNAL_SESSION_SERVICE} ${NAMESPACE} ${RELEASE} ${SESSION_FRONTEND_PORT} ${SESSION_NODE_PORT} ${CACHE_CA_SECRET} ${CACHE_TLS_SECRET} ${CACHE_GATEWAY_HOST} ${CACHE_CERTIFICATE_IPS} ${ENVOY_PROXY} ${CACHE_GATEWAY_NODE_PORT} ${GATEWAY_CLASS} ${GATEWAY} ${CACHE_BOOTSTRAP_ROUTE} ${CACHE_SERVICE} ${CACHE_FLIGHT_PORT} ${CACHE_BACKEND} ${CACHE_OWNER_ROUTE} ${CACHE_SECURITY_POLICY} ${CACHE_AUTHORITY_EXPRESSION}' \
        <"$EXTERNAL_ACCESS_TEMPLATE" >"$RENDERED_EXTERNAL_ACCESS"
    kubectl apply -f "$RENDERED_EXTERNAL_ACCESS"

    kubectl -n "$NAMESPACE" wait \
        --for=condition=Ready "certificate/${RELEASE}-cache-server" \
        --timeout="$TIMEOUT"
    kubectl -n "$NAMESPACE" wait \
        --for=condition=Programmed "gateway/${GATEWAY}" \
        --timeout="$TIMEOUT"
    wait_backend_accepted
    wait_route_ready "$CACHE_BOOTSTRAP_ROUTE"
    wait_route_ready "$CACHE_OWNER_ROUTE"
    wait_security_policy_accepted

    # Confirm the generated Service has the fixed NodePort mapped by ci/k8s/kind.yaml.
    kubectl -n "$ENVOY_GATEWAY_NAMESPACE" get service \
        -l "gateway.envoyproxy.io/owning-gateway-namespace=${NAMESPACE},gateway.envoyproxy.io/owning-gateway-name=${GATEWAY}" \
        -o jsonpath="{.items[0].spec.ports[?(@.nodePort==${CACHE_GATEWAY_NODE_PORT})].nodePort}" \
        | grep -qx "${CACHE_GATEWAY_NODE_PORT}"

    # Referencing the internal Service above is intentional for bootstrap calls.
    # Object refs carry podIP:port authorities, which the second route resolves
    # dynamically and the SecurityPolicy restricts to current cache replicas.
}

configure_external_access() {
    local session_service=""
    local cache_service=""
    local session_frontend_port=""
    local cache_flight_port=""

    session_service="$(kubectl -n "$NAMESPACE" get service \
        -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=session-manager" \
        -o jsonpath='{.items[0].metadata.name}')"
    cache_service="$(kubectl -n "$NAMESPACE" get service \
        -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=object-cache" \
        -o jsonpath='{.items[0].metadata.name}')"
    session_frontend_port="$(kubectl -n "$NAMESPACE" get service "$session_service" \
        -o jsonpath='{.spec.ports[?(@.name=="frontend")].port}')"
    cache_flight_port="$(kubectl -n "$NAMESPACE" get service "$cache_service" \
        -o jsonpath='{.spec.ports[?(@.name=="flight")].port}')"
    : "${session_frontend_port:?missing frontend port on service ${session_service}}"
    : "${cache_flight_port:?missing flight port on service ${cache_service}}"

    create_external_access \
        "$cache_service" \
        "$session_frontend_port" \
        "$cache_flight_port"
}

extract_host_client() {
    CLIENT_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/flame-k8s-e2e.XXXXXX")"
    CLIENT_CONTAINER="$(docker create "${IMAGE_REGISTRY}/flame-console:${IMAGE_TAG}")"

    log "Extracting Flame client from the console image"
    docker cp "${CLIENT_CONTAINER}:/usr/local/flame" "$CLIENT_ROOT/"
    docker rm -f "$CLIENT_CONTAINER" >/dev/null
    CLIENT_CONTAINER=""

    export FLAME_HOME="${CLIENT_ROOT}/flame"
    export PYTHONPATH="${PYTHONPATH:-}"
    export LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-}"
    sed -i "s|/usr/local/flame|${FLAME_HOME}|g" "${FLAME_HOME}/sbin/flmenv.sh"
    # shellcheck disable=SC1091
    source "${FLAME_HOME}/sbin/flmenv.sh"
}

configure_host_client() {
    local ca_file="${CLIENT_ROOT}/cache-ca.crt"
    local config_dir="${CLIENT_ROOT}/home/.flame"
    local config_file="${config_dir}/flame.yaml"

    kubectl -n "$NAMESPACE" get secret "$CACHE_CA_SECRET" \
        -o jsonpath='{.data.tls\.crt}' | base64 --decode >"$ca_file"

    mkdir -p "$config_dir"
    export CACHE_CA_FILE="$ca_file"
    export RELEASE SESSION_NODE_PORT CACHE_GATEWAY_HOST CACHE_GATEWAY_NODE_PORT
    envsubst '${RELEASE} ${SESSION_NODE_PORT} ${CACHE_GATEWAY_HOST} ${CACHE_GATEWAY_NODE_PORT} ${CACHE_CA_FILE}' \
        <"$VM_CLIENT_CONFIG_TEMPLATE" >"$config_file"

    export HOME="${CLIENT_ROOT}/home"
    export FLAME_ENDPOINT="http://127.0.0.1:${SESSION_NODE_PORT}"
    export FLAME_CACHE_ENDPOINT="grpcs-proxy://${CACHE_GATEWAY_HOST}:${CACHE_GATEWAY_NODE_PORT}"
    export FLAME_CA_FILE="$ca_file"

    if ! grep -qwF "$CACHE_GATEWAY_HOST" /etc/hosts; then
        printf '127.0.0.1 %s\n' "$CACHE_GATEWAY_HOST" \
            | sudo --non-interactive tee -a /etc/hosts >/dev/null
    fi
}

run_smoke_tests() {
    log "Running Flame smoke tests directly from the VM"
    flmctl --config "${HOME}/.flame/flame.yaml" list -a
    flmctl --config "${HOME}/.flame/flame.yaml" list -n
    flmping -t "$FLMPING_TASKS"
    PYTHONPATH="${ROOT_DIR}/e2e/src:${PYTHONPATH:-}" python3 -m e2e.app \
        --name "${RELEASE}-app-e2e" \
        --tasks "$APP_E2E_TASKS" \
        --json

    pushd "${FLAME_HOME}/examples/pi/python" >/dev/null
    PI_NUM_BATCHES="$PI_NUM_BATCHES" \
        PI_SAMPLES_PER_BATCH="$PI_SAMPLES_PER_BATCH" \
        uv run main.py
    popd >/dev/null
}

run_e2e_tests() {
    log "Running E2E tests directly from the VM"
    pushd "${ROOT_DIR}/e2e" >/dev/null
    PYTHONPATH="${ROOT_DIR}/e2e/src:${PYTHONPATH}" \
        FLAME_LOG=DEBUG \
        uv run --no-project \
        --with pytest \
        --with pytest-timeout \
        python -m pytest -vv --durations=0 \
        tests/test_app.py \
        tests/test_flmexec.py
    popd >/dev/null
}

run_vm_tests() {
    local suite="$1"

    extract_host_client
    configure_host_client
    case "$suite" in
        smoke) run_smoke_tests ;;
        e2e) run_e2e_tests ;;
        all)
            run_smoke_tests
            run_e2e_tests
            ;;
    esac
}

usage() {
    cat <<'EOF'
Usage: ci/k8s/e2e.sh [command] [helm arguments]

Commands:
  install-infrastructure  Install cert-manager and Envoy Gateway.
  install-flame           Lint, render, and install Flame; accepts Helm arguments.
  configure-access        Configure session and cache access from the VM.
  run-smoke-tests         Run lightweight Flame client tests from the VM.
  run-e2e-tests           Run the App and flmexec E2E cases from the VM.
  diagnostics             Print Kubernetes and component diagnostics.
  all                     Run every phase; accepts Helm arguments (default).
EOF
}

require_no_arguments() {
    local command="$1"
    shift
    if [[ "$#" -ne 0 ]]; then
        log "${command} does not accept additional arguments"
        usage
        return 2
    fi
}

COMMAND="${1:-all}"
case "$COMMAND" in
    -h | --help)
        usage
        exit 0
        ;;
    install-infrastructure | install-flame | configure-access | run-smoke-tests | run-e2e-tests | diagnostics | all)
        if [[ "$#" -gt 0 ]]; then
            shift
        fi
        ;;
    -*)
        COMMAND="all"
        ;;
    *)
        log "Unknown command: ${COMMAND}"
        usage
        exit 2
        ;;
esac

trap finish EXIT

case "$COMMAND" in
    install-infrastructure)
        require_no_arguments "$COMMAND" "$@"
        install_external_infrastructure
        ;;
    install-flame)
        install_flame "$@"
        ;;
    configure-access)
        require_no_arguments "$COMMAND" "$@"
        command -v envsubst >/dev/null || { log "envsubst is required"; exit 1; }
        configure_external_access
        ;;
    run-smoke-tests)
        require_no_arguments "$COMMAND" "$@"
        command -v envsubst >/dev/null || { log "envsubst is required"; exit 1; }
        run_vm_tests smoke
        ;;
    run-e2e-tests)
        require_no_arguments "$COMMAND" "$@"
        command -v envsubst >/dev/null || { log "envsubst is required"; exit 1; }
        run_vm_tests e2e
        ;;
    diagnostics)
        require_no_arguments "$COMMAND" "$@"
        dump_debug
        ;;
    all)
        command -v envsubst >/dev/null || { log "envsubst is required"; exit 1; }
        install_external_infrastructure
        install_flame "$@"
        configure_external_access
        run_vm_tests all
        ;;
esac

log "Kubernetes e2e ${COMMAND} completed"
