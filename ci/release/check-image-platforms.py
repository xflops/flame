#!/usr/bin/env python3
import json
import sys


def add_platform(platforms, os_name, architecture):
    if not os_name or not architecture:
        return
    if os_name == "unknown" or architecture == "unknown":
        return
    platforms.add(f"{os_name}/{architecture}")


def manifest_platforms(data):
    if isinstance(data, list):
        data = data[0] if data else {}

    platforms = set()
    for manifest in data.get("manifests", []):
        platform = manifest.get("platform", {})
        add_platform(platforms, platform.get("os"), platform.get("architecture"))

    add_platform(platforms, data.get("os"), data.get("architecture"))
    add_platform(platforms, data.get("Os"), data.get("Architecture"))
    return platforms


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: check-image-platforms.py IMAGE EXPECTED_PLATFORMS")

    image = sys.argv[1]
    expected = set(sys.argv[2].replace(",", " ").split())
    data = json.load(sys.stdin)
    platforms = manifest_platforms(data)
    missing = expected - platforms

    print(f"{image}: platforms={','.join(sorted(platforms)) or 'unknown'}")
    if missing:
        raise SystemExit(
            f"{image}: missing expected platforms {','.join(sorted(missing))}"
        )


if __name__ == "__main__":
    main()
