#!/usr/bin/env bash
# Longevity test: keep a Flame cluster busy with flmbench bursts for a long time.
#
# Every iteration runs one flmbench burst (SESSIONS sessions x TASKS tasks). The
# test fails on the first burst in which a task fails or a node is not ready, so
# slow leaks (file descriptors, memory, stuck executors) show up as a failure
# some hours in rather than as a slower run.
#
# Usage: DURATION=8h SESSIONS=10 TASKS=3000 testing/system/longevity/run.sh
set -euo pipefail

DURATION="${DURATION:-1h}"        # total time; a number with an optional s, m or h suffix
SESSIONS="${SESSIONS:-10}"        # sessions per burst
TASKS="${TASKS:-3000}"            # tasks per session
PAUSE="${PAUSE:-10}"              # seconds between bursts
OUTPUT_DIR="${OUTPUT_DIR:-longevity-results}"

seconds_of() {
    case "$1" in
        *h) echo $(( ${1%h} * 3600 )) ;;
        *m) echo $(( ${1%m} * 60 )) ;;
        *s) echo "${1%s}" ;;
        *) echo "$1" ;;
    esac
}

command -v flmbench >/dev/null || { echo "flmbench not found in PATH" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq not found in PATH" >&2; exit 2; }

mkdir -p "$OUTPUT_DIR"
deadline=$(( $(date +%s) + $(seconds_of "$DURATION") ))
iteration=0

while [ "$(date +%s)" -lt "$deadline" ]; do
    iteration=$(( iteration + 1 ))
    result="$OUTPUT_DIR/burst-$(printf '%04d' "$iteration").json"
    echo "== burst $iteration: $SESSIONS x $TASKS tasks, $(date -u +%FT%TZ) =="
    if ! flmbench -s "$SESSIONS" -t "$TASKS" -o "$result"; then
        echo "FAIL: burst $iteration did not finish all tasks" >&2
        exit 1
    fi
    # Any health sample with a node not ready fails the run.
    not_ready=$(jq '[.rounds[].health.not_ready_samples] | add' "$result")
    if [ "$not_ready" -gt 0 ]; then
        echo "FAIL: burst $iteration had $not_ready health sample(s) with a node not ready" >&2
        exit 1
    fi
    sleep "$PAUSE"
done

echo "PASS: $iteration burst(s) in $DURATION"
