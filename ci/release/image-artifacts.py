#!/usr/bin/env python3
"""Archive native CI images and publish those exact images after E2E succeeds."""

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path

IMAGES = (
    "flame-session-manager",
    "flame-object-cache",
    "flame-executor-manager-runtime",
    "flame-executor-manager-devel",
    "flame-console",
    "flame-test",
    "flame-instance-runtime",
    "flame-instance-runtime-devel",
)
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")


def repository(image):
    if image in {"flame-executor-manager-runtime", "flame-executor-manager-devel"}:
        return "flame-executor-manager"
    return image.removesuffix("-devel")


def tag(image, base, arch):
    return f"{base}{'-devel' if image.endswith('-devel') else ''}-{arch}"


def inspect(refs):
    result = subprocess.run(["docker", "image", "inspect", *refs], check=True, capture_output=True, text=True)
    values = json.loads(result.stdout)
    if len(values) != len(refs):
        raise ValueError("Docker did not inspect every required image")
    return values


def validate(values, arch, sha, expected=None):
    for image, value in zip(IMAGES, values, strict=True):
        if value.get("Architecture") != arch or value.get("Os") != "linux":
            raise ValueError(f"wrong platform for {image}")
        if value.get("Config", {}).get("Labels", {}).get("org.opencontainers.image.revision") != sha:
            raise ValueError(f"wrong source revision for {image}")
        identity = value.get("Id", "")
        if not DIGEST.fullmatch(identity):
            raise ValueError(f"invalid image ID for {image}")
        if expected is not None and identity != expected[image]["id"]:
            raise ValueError(f"image ID changed for {image}")


def pipeline(producer, consumer, output=None):
    process = subprocess.Popen(producer, stdout=subprocess.PIPE)
    try:
        result = subprocess.run(consumer, stdin=process.stdout, stdout=output, check=False)
    finally:
        process.stdout.close()
        status = process.wait()
    if status or result.returncode:
        raise RuntimeError(f"artifact pipeline failed: {producer[0]}={status}, {consumer[0]}={result.returncode}")


def checksum(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def smoke(image, ref, arch):
    base = ["docker", "run", "--rm", "--platform", f"linux/{arch}"]
    if image == "flame-console":
        command = ["--entrypoint", "/usr/local/flame/bin/flmctl", ref, "--help"]
    elif image == "flame-test":
        command = [ref, "flame-test", "help"]
    elif image.startswith("flame-instance-runtime"):
        command = ["--entrypoint", "/usr/local/flame/bin/uv", ref, "--version"]
    else:
        command = [ref, "--help"]
    subprocess.run(base + command, check=True)
    if image.endswith("-devel"):
        subprocess.run(base + ["--entrypoint", "/bin/bash", ref, "-lc", 'gcc --version >/dev/null && echo "#include <Python.h>" | gcc -E $(python3-config --includes) - >/dev/null'], check=True)
    if image not in {"flame-session-manager", "flame-object-cache"}:
        subprocess.run(base + ["--entrypoint", "/bin/bash", ref, "-lc", 'source /usr/local/flame/sbin/flmenv.sh --python-version 3.12 && python3 -c "import flamepy"'], check=True)


def save(registry, arch, sha, directory):
    refs = [f"{registry}/{repository(image)}:{tag(image, 'ci', arch)}" for image in IMAGES]
    values = inspect(refs)
    validate(values, arch, sha)
    aliases = []
    for image, ref in zip(IMAGES, refs, strict=True):
        smoke(image, ref, arch)
        compatibility = f"{registry}/{repository(image)}:ci{'-devel' if image.endswith('-devel') else ''}"
        subprocess.run(["docker", "tag", ref, compatibility], check=True)
        aliases.append(compatibility)
        if image.startswith("flame-instance-runtime"):
            runtime = f"xflops/flmrt:ci{'-devel' if image.endswith('-devel') else ''}"
            subprocess.run(["docker", "tag", ref, runtime], check=True)
            aliases.append(runtime)
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / "flame-images.tar.zst"
    with archive.open("wb") as output:
        pipeline(["docker", "save", *refs, *aliases], ["zstd", "-T0", "-3"], output)
    manifest = {
        "source_sha": sha,
        "architecture": arch,
        "archive_sha256": checksum(archive),
        "images": {image: {"ref": ref, "id": value["Id"]} for image, ref, value in zip(IMAGES, refs, values, strict=True)},
    }
    (directory / "images.json").write_text(json.dumps(manifest, indent=2) + "\n")


def push(registry, arch, sha, directory):
    manifest = json.loads((directory / "images.json").read_text())
    if manifest.get("source_sha") != sha or manifest.get("architecture") != arch:
        raise ValueError("artifact source revision or architecture mismatch")
    images = manifest["images"]
    if set(images) != set(IMAGES):
        raise ValueError("artifact does not contain the complete image set")
    refs = [f"{registry}/{repository(image)}:{tag(image, 'ci', arch)}" for image in IMAGES]
    for image, ref in zip(IMAGES, refs, strict=True):
        if images[image].get("ref") != ref or not DIGEST.fullmatch(images[image].get("id", "")):
            raise ValueError(f"invalid artifact reference or ID for {image}")
    archive = directory / "flame-images.tar.zst"
    if checksum(archive) != manifest["archive_sha256"]:
        raise ValueError("artifact archive checksum mismatch")
    pipeline(["zstd", "-dc", str(archive)], ["docker", "load"])
    # Check the entire loaded image set before any registry writes.
    validate(inspect(refs), arch, sha, images)
    digests = {}
    for image, ref in zip(IMAGES, refs, strict=True):
        repo = f"{registry}/{repository(image)}"
        destination = f"{repo}:{tag(image, 'sha-' + sha, arch)}"
        subprocess.run(["docker", "tag", ref, destination], check=True)
        subprocess.run(["docker", "push", destination], check=True)
        value = inspect([destination])[0]
        matches = {item.rsplit("@", 1)[1] for item in value.get("RepoDigests", []) if item.rsplit("@", 1)[0].removeprefix("docker.io/") == repo.removeprefix("docker.io/") and "@" in item}
        if len(matches) != 1 or not DIGEST.fullmatch(next(iter(matches))):
            raise ValueError(f"Docker returned no unique pushed digest for {destination}")
        digests[image] = {"containerimage.digest": matches.pop()}
    (directory / f"digests-{arch}.json").write_text(json.dumps(digests, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("save", "push"))
    parser.add_argument("registry")
    parser.add_argument("arch", choices=("amd64", "arm64"))
    parser.add_argument("sha")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        parser.error("source SHA must be a full Git commit ID")
    action = save if args.mode == "save" else push
    action(args.registry, args.arch, args.sha, args.directory)


if __name__ == "__main__":
    main()
