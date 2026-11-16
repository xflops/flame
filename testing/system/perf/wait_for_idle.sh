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

# Every page must be inspected: a first page of retained history cannot prove idle.
count_matching() {
    local resource=$1 predicate=$2 number=1 count=0
    local page metadata limit total matches
    while true; do
        page=$(flmctl list "$resource" -o json --number "$number") || return
        metadata=$(jq -er '.paging | select(.limit > 0 and .number > 0 and .total != null) | [.limit, .total] | @tsv' <<< "$page") || return
        read -r limit total <<< "$metadata" || return
        matches=$(jq -r "[.items[] | select($predicate)] | length" <<< "$page") || return
        count=$((count + matches))
        if (( number * limit >= total )); then
            echo "$count"
            return
        fi
        number=$((number + 1))
    done
}

while true; do
    open_sessions=$(count_matching -s '.state == "Open"')
    unreleased_executors=$(count_matching -e '.state != "Released"')

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
