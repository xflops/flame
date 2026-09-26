#!/usr/bin/env bash
# Copyright 2026 The Flame Authors.
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#     http://www.apache.org/licenses/LICENSE-2.0
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -euo pipefail

timeout_seconds=${1:-180}
if [[ ! "$timeout_seconds" =~ ^[0-9]+$ ]]; then
    echo "Idle timeout must be a non-negative number of seconds" >&2
    exit 2
fi
deadline=$((SECONDS + timeout_seconds))
last_counts=""

while true; do
    open_sessions=$(flmctl list -s -o json | jq -r '[.[] | select(.state == "Open")] | length')
    unreleased_executors=$(flmctl list -e -o json | jq -r '[.[] | select(.state != "Released")] | length')

    if (( open_sessions == 0 && unreleased_executors == 0 )); then
        echo "Benchmark cluster is idle: no open sessions or retained executors"
        exit 0
    fi

    counts="$open_sessions open sessions, $unreleased_executors unreleased executors"
    if [[ "$counts" != "$last_counts" ]]; then
        echo "Waiting for benchmark cluster: $counts"
        last_counts="$counts"
    fi
    if (( SECONDS >= deadline )); then
        echo "Benchmark cluster did not become idle: $counts" >&2
        exit 1
    fi
    sleep 1
done
