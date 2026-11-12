#!/usr/bin/env bash
# Decide whether a run may replace an output shared by releases or main commits.
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
    echo "usage: $0 main SOURCE_SHA | release RELEASE_TAG" >&2
    exit 2
fi

mode="$1"
expected="$2"
case "$mode" in
    main)
        current="$(gh api "repos/${GITHUB_REPOSITORY}/commits/main" --jq .sha)"
        ;;
    release)
        # Release candidates always retain their versioned installers only.
        if [[ "$expected" == *-* ]]; then
            echo false
            exit 0
        fi
        current="$(gh api "repos/${GITHUB_REPOSITORY}/releases/latest" --jq .tag_name)"
        ;;
    *)
        echo "unknown publication mode: $mode" >&2
        exit 2
        ;;
esac

if [[ -z "$current" || "$current" == null ]]; then
    echo "GitHub returned no current $mode identity" >&2
    exit 1
fi
if [[ "$current" == "$expected" ]]; then
    echo true
else
    echo "Skipping shared $mode output: current identity is $current, run identity is $expected" >&2
    echo false
fi
