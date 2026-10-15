#!/usr/bin/env bash
# Stress test: push a Flame cluster well past its steady load with flmbench.
#
# The test runs a ladder of bursts. Each step multiplies the work of the last
# (more sessions, more tasks per session, then larger task payloads) and must
# finish every task with every node ready. After the last step it checks that the
# cluster recovered: a small burst still succeeds with no node not ready.
#
# Usage: PROFILE=default testing/system/stress/run.sh
set -euo pipefail

PROFILE="${PROFILE:-default}"       # smoke, default or heavy
OUTPUT_DIR="${OUTPUT_DIR:-stress-results}"
TIMEOUT="${TIMEOUT:-900}"           # seconds to wait for each burst, passed to flmbench

# Each step is "sessions tasks-per-session [extra flmbench options]".
case "$PROFILE" in
    smoke)
        STEPS=("2 100" "4 250" "8 250") ;;
    default)
        STEPS=("1 10000" "10 3000" "50 1000" "100 500" "20 2000 -m 64MiB -d 100") ;;
    heavy)
        STEPS=("1 50000" "10 10000" "100 2000" "200 1000" "50 2000 -m 256MiB -d 500") ;;
    *)
        echo "PROFILE must be smoke, default or heavy, got '$PROFILE'" >&2; exit 2 ;;
esac

command -v flmbench >/dev/null || { echo "flmbench not found in PATH" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq not found in PATH" >&2; exit 2; }
mkdir -p "$OUTPUT_DIR"

# burst NAME SESSIONS TASKS [flmbench options]: fail unless every task succeeds
# and no health sample finds a node not ready.
burst() {
    local name="$1" sessions="$2" tasks="$3"
    shift 3
    local result="$OUTPUT_DIR/$name.json"
    echo "== $name: $sessions x $tasks tasks $* =="
    if ! flmbench -s "$sessions" -t "$tasks" --timeout "$TIMEOUT" -o "$result" "$@"; then
        echo "FAIL: $name did not finish all tasks" >&2
        exit 1
    fi
    local not_ready
    not_ready=$(jq '[.rounds[].health.not_ready_samples] | add' "$result")
    if [ "$not_ready" -gt 0 ]; then
        echo "FAIL: $name had $not_ready health sample(s) with a node not ready" >&2
        exit 1
    fi
}

step=0
for spec in "${STEPS[@]}"; do
    step=$(( step + 1 ))
    # shellcheck disable=SC2086
    burst "step-$(printf '%02d' "$step")" $spec
done

burst recovery 2 100

echo "PASS: $PROFILE profile, $step step(s) and recovery"
