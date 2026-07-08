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
RUNNER_E2E_TASKS="${RUNNER_E2E_TASKS:-3}"
PI_NUM_BATCHES="${PI_NUM_BATCHES:-2}"
PI_SAMPLES_PER_BATCH="${PI_SAMPLES_PER_BATCH:-1000}"
E2E_POD="${E2E_POD:-${RELEASE}-console-e2e}"

HELM_E2E_ARGS=(
    --set "global.imageRegistry=${IMAGE_REGISTRY}"
    --set "global.imageTag=${IMAGE_TAG}"
    --set "global.imagePullPolicy=${IMAGE_PULL_POLICY}"
    --set "cluster.storage=${SESSION_MANAGER_STORAGE}"
    --set "cluster.policies[0]=drf"
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

    log "Recent events"
    kubectl -n "$NAMESPACE" get events --sort-by=.lastTimestamp 2>/dev/null || true

    for component in session-manager object-cache executor-manager test; do
        log "Logs for component=${component}"
        kubectl -n "$NAMESPACE" logs -l "app.kubernetes.io/component=${component}" --all-containers --tail=500 2>/dev/null || true
    done

    log "Console e2e pod"
    kubectl -n "$NAMESPACE" describe pod "$E2E_POD" 2>/dev/null || true
    kubectl -n "$NAMESPACE" logs "$E2E_POD" --all-containers --tail=500 2>/dev/null || true
}

wait_rollout() {
    local kind="$1"
    local component="$2"

    log "Waiting for ${kind}/${component}"
    kubectl -n "$NAMESPACE" rollout status "$kind" \
        -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${component}" \
        --timeout="$TIMEOUT"
}

trap 'rc=$?; if [ "$rc" -ne 0 ]; then dump_debug; fi' EXIT

log "Linting chart"
helm lint "$CHART_DIR" "${HELM_E2E_ARGS[@]}" "$@"

log "Rendering chart"
helm template "$RELEASE" "$CHART_DIR" \
    --namespace "$NAMESPACE" \
    "${HELM_E2E_ARGS[@]}" \
    "$@" >/tmp/flame-k8s-e2e-rendered.yaml

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

log "Running Helm chart test"
helm test "$RELEASE" --namespace "$NAMESPACE" --timeout "$TIMEOUT" --logs

CONFIG_MAP="$(kubectl -n "$NAMESPACE" get configmap \
    -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/name=flame" \
    -o jsonpath='{.items[0].metadata.name}')"
SESSION_SERVICE="$(kubectl -n "$NAMESPACE" get service \
    -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=session-manager" \
    -o jsonpath='{.items[0].metadata.name}')"
CACHE_SERVICE="$(kubectl -n "$NAMESPACE" get service \
    -l "app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=object-cache" \
    -o jsonpath='{.items[0].metadata.name}')"
SESSION_FRONTEND_PORT="$(kubectl -n "$NAMESPACE" get service "$SESSION_SERVICE" \
    -o jsonpath='{.spec.ports[?(@.name=="frontend")].port}')"
CACHE_FLIGHT_PORT="$(kubectl -n "$NAMESPACE" get service "$CACHE_SERVICE" \
    -o jsonpath='{.spec.ports[?(@.name=="flight")].port}')"
: "${SESSION_FRONTEND_PORT:?missing frontend port on service ${SESSION_SERVICE}}"
: "${CACHE_FLIGHT_PORT:?missing flight port on service ${CACHE_SERVICE}}"

log "Running in-cluster console e2e pod"
kubectl -n "$NAMESPACE" delete pod "$E2E_POD" --ignore-not-found=true >/dev/null
cat <<EOF | kubectl -n "$NAMESPACE" apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: ${E2E_POD}
  labels:
    app.kubernetes.io/name: flame
    app.kubernetes.io/instance: ${RELEASE}
    app.kubernetes.io/component: test
spec:
  restartPolicy: Never
  containers:
    - name: console
      image: ${IMAGE_REGISTRY}/flame-console:${IMAGE_TAG}
      imagePullPolicy: ${IMAGE_PULL_POLICY}
      command:
        - /bin/bash
        - -ec
      args:
        - |
          source /usr/local/flame/sbin/flmenv.sh
          mkdir -p /root/.flame
          cp /etc/flame/flame.yaml /root/.flame/flame.yaml
          export FLAME_ENDPOINT=http://${SESSION_SERVICE}:${SESSION_FRONTEND_PORT}
          export FLAME_CACHE_ENDPOINT=grpc://${CACHE_SERVICE}:${CACHE_FLIGHT_PORT}
          flmctl --config /root/.flame/flame.yaml list -a
          flmctl --config /root/.flame/flame.yaml list -n
          flmping -t ${FLMPING_TASKS}
          python3 -m flamepy.runner.e2e --name ${RELEASE}-runner-e2e --tasks ${RUNNER_E2E_TASKS} --json
          cd /usr/local/flame/examples/pi/python
          export PI_NUM_BATCHES=${PI_NUM_BATCHES}
          export PI_SAMPLES_PER_BATCH=${PI_SAMPLES_PER_BATCH}
          uv run main.py
      volumeMounts:
        - name: client-config
          mountPath: /etc/flame/flame.yaml
          subPath: flame.yaml
          readOnly: true
  volumes:
    - name: client-config
      configMap:
        name: ${CONFIG_MAP}
EOF

if ! kubectl -n "$NAMESPACE" wait --for=jsonpath='{.status.phase}'=Succeeded "pod/${E2E_POD}" --timeout="$TIMEOUT"; then
    log "Console e2e pod failed"
    kubectl -n "$NAMESPACE" logs "$E2E_POD" --all-containers --tail=500 || true
    exit 1
fi

kubectl -n "$NAMESPACE" logs "$E2E_POD" --all-containers
log "Kubernetes e2e completed"
