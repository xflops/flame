# Flame Release Process

This runbook is written for a release agent. Follow it as a checklist and record
the exact versions, commit SHAs, registry URLs, and verification results before
calling the release complete.

## Release Inputs

Set these values at the start of every release:

```shell
export RELEASE_TAG=v0.6.0-rc1
export CARGO_VERSION=0.6.0-rc1
export PYTHON_VERSION=0.6.0rc1
export STDNG_VERSION=0.1.8
export DOCKER_TAG="${RELEASE_TAG}"
export RELEASE_BRANCH=release-0.6
export IMAGE_REGISTRY=docker.io/xflops
export RELEASE_IMAGE_PLATFORMS=linux/amd64,linux/arm64
export RUST_BUILDER_IMAGE=docker.io/library/rust:1.95
export UBUNTU_BASE_IMAGE=docker.io/library/ubuntu:24.04
# Optional override; Makefile auto-detects usable podman first, then docker.
# export CONTAINER_CLI=podman
export RELEASE_NOTES_FILE=/tmp/flame-${RELEASE_TAG}-notes.md
```

Version conventions:

- GitHub release tags and Docker tags use the leading `v`: `v0.6.0-rc1`.
- Cargo package versions omit the leading `v`: `0.6.0-rc1`.
- PyPI versions follow PEP 440: `0.6.0rc1` instead of `0.6.0-rc1`.
- Stable releases use `v0.6.0`, `0.6.0`, and `0.6.0`.
- Published Cargo and PyPI versions are immutable. If one artifact has already
  been published incorrectly, bump the next release candidate and document the
  mapping instead of overwriting.

## Required Access

Confirm source and package credentials before changing versions:

```shell
git fetch upstream --tags --prune
git status --short
gh auth status
cargo owner --list flame-rs
test -f ~/.pypirc || test -n "${UV_PUBLISH_TOKEN:-}" || test -n "${UV_PUBLISH_PASSWORD:-}"
```

Confirm container image credentials with the tool selected for the image publish
step:

```shell
make release-images-login
```

Required permissions:

- Push access to `upstream` and the release branch.
- GitHub release/tag permission for `xflops/flame`.
- crates.io publish permission for `stdng`, `flame-rs-macros`, and `flame-rs`.
- PyPI publish permission for `flamepy`.
- Docker Hub publish permission for `xflops/flame-*` repositories.

Keep local `tasks/` notes out of commits and container build contexts.
`.dockerignore` should include `tasks/`.

## Source Readiness

1. Start from the release branch or create it from the intended base:

   ```shell
   git switch "${RELEASE_BRANCH}"
   git pull --ff-only upstream "${RELEASE_BRANCH}"
   ```

2. Confirm all required source PRs have landed on `main` first. Release branch
   changes should usually be cherry-picks from main, not release-only feature
   work.

3. For each backport PR, verify cherry-pick hygiene:

   ```shell
   git range-diff <source-sha>^..<source-sha> <backport-sha>^..<backport-sha>
   git show <source-sha> -- | git patch-id --stable
   git show <backport-sha> -- | git patch-id --stable
   ```

   The backport should contain only the intended cherry-picked commits plus the
   standard cherry-pick trailer. Do not mix release notes or unrelated fixes into
   a mechanical backport PR.

4. Verify CI is green for the release branch. At minimum, inspect GitHub checks
   for the final release commit and confirm Kubernetes E2E passed when the
   release includes Helm or image changes.

## Version Metadata

Update the release versions in the files that apply to the release:

- `sdk/python/pyproject.toml`: `project.version = "<PYTHON_VERSION>"`
- `sdk/python/src/flamepy/__init__.py`: `__version__ = "<PYTHON_VERSION>"`
- `sdk/python/README.md`: install examples or version references, if present
- `sdk/rust/macros/Cargo.toml`: `version = "<CARGO_VERSION>"`
- `stdng/Cargo.toml`: bump only when publishing stdng changes
- `sdk/rust/Cargo.toml`: `version = "<CARGO_VERSION>"`
- `sdk/rust/Cargo.toml`: dependency versions for `stdng` and
  `flame-rs-macros`
- `charts/flame/Chart.yaml`: `appVersion` for the Flame release
- `Cargo.lock`: refresh after Cargo metadata changes

Publish order matters for Rust:

1. `stdng`, if its version changed.
2. `flame-rs-macros`.
3. `flame-rs`, after its registry dependencies exist.

Do not publish `flame-rs` with duplicated helper code if the helpers belong in
`stdng`; publish the required `stdng` version first and depend on it from
`sdk/rust/Cargo.toml`.

## Release Notes

Create the release notes before publishing so package and GitHub metadata use
the same wording. The notes should include:

- The source comparison range, previous release tag, and target commit.
- User-facing highlights, breaking changes, and upgrade notes.
- Package, crate, Docker image, and Helm chart version values.
- Known gaps, such as local smoke tests that could not be run.
- Links to source PRs. Backport PRs should be cited only when the backport
  itself has user-facing behavior.

Write the final body to `${RELEASE_NOTES_FILE}` and use that same file for the
GitHub release.

## Local Verification

Run the focused checks before publishing anything:

```shell
cargo fmt --check
cargo check -p stdng
cargo check -p flame-rs --features macros
cargo package --manifest-path stdng/Cargo.toml --allow-dirty
cargo package --manifest-path sdk/rust/macros/Cargo.toml --allow-dirty
cargo package --manifest-path sdk/rust/Cargo.toml --allow-dirty --features macros
```

If `flame-rs` depends on a `flame-rs-macros` version that has not been
published yet, the final `flame-rs` package verification will fail while
resolving registry dependencies. In that case, publish and verify
`flame-rs-macros` first, then rerun the full `flame-rs` package command before
publishing `flame-rs`.

Python package verification:

```shell
cd sdk/python
uv run -n --extra dev pytest tests/test_runner_e2e.py tests/test_runner.py -q
uv run -n python -c 'import flamepy; print(flamepy.__version__)'
uv build --out-dir /tmp/flamepy-${PYTHON_VERSION}-dist
cd -
```

Repository checks:

```shell
git diff --check
bash -n ci/k8s/e2e.sh
python3 -m json.tool charts/flame/values.schema.json
```

The non-publishing release sanity script wraps the metadata, local, package, and
optional artifact checks:

```shell
make release-sanity
```

If `helm` and a local Kubernetes backend are available, also run:

```shell
helm lint charts/flame
helm template flame charts/flame --set global.imageTag="${DOCKER_TAG}"
```

## Publish Python Package

Build artifacts from `sdk/python` and publish exactly those files:

```shell
cd sdk/python
uv build --out-dir /tmp/flamepy-${PYTHON_VERSION}-dist
uv publish /tmp/flamepy-${PYTHON_VERSION}-dist/*
cd -
```

If `uv publish` cannot find credentials, set `UV_PUBLISH_USERNAME` and
`UV_PUBLISH_PASSWORD` from a secure source. Do not print tokens or `.pypirc`
contents in logs.

Verify PyPI:

```shell
curl -fsSL "https://pypi.org/pypi/flamepy/${PYTHON_VERSION}/json" \
  | python3 -m json.tool
curl -fsSL "https://pypi.org/simple/flamepy/" | rg "${PYTHON_VERSION}"
```

For release candidates, PyPI may keep the project-level latest version on the
latest stable release. That is expected.

## Publish Rust Crates

Publish crates in dependency order. Skip the `stdng` publish command if the
required `${STDNG_VERSION}` already exists on crates.io and no stdng changes are
part of the release.

```shell
cargo publish --manifest-path stdng/Cargo.toml --allow-dirty
cargo publish --manifest-path sdk/rust/macros/Cargo.toml --allow-dirty
cargo publish --manifest-path sdk/rust/Cargo.toml --allow-dirty --features macros
```

After each publish, wait for the registry to expose the version before publishing
the dependent crate:

```shell
curl -fsSL "https://crates.io/api/v1/crates/stdng/${STDNG_VERSION}" \
  | python3 -m json.tool
curl -fsSL "https://crates.io/api/v1/crates/flame-rs-macros/${CARGO_VERSION}" \
  | python3 -m json.tool
curl -fsSL "https://crates.io/api/v1/crates/flame-rs/${CARGO_VERSION}" \
  | python3 -m json.tool
```

For `flame-rs`, also verify the crates.io dependency list includes the expected
published `stdng` and `flame-rs-macros` versions.

## Build And Publish Docker Images

Release Docker images as multi-arch manifest tags for `linux/amd64` and
`linux/arm64`. The public repositories are:

- `xflops/flame-session-manager`
- `xflops/flame-object-cache`
- `xflops/flame-executor-manager`
- `xflops/flame-console`

Do not move `latest` for release candidates. For stable releases, move `latest`
only after the versioned tag has been pushed and verified.

Build release images with manifest lists. The Makefile detects a
Docker-compatible `CONTAINER_CLI` from the host, preferring a usable Docker
daemon and falling back to Podman when Docker is unavailable. Set
`CONTAINER_CLI=docker` or `CONTAINER_CLI=podman` when you need to override that
choice. The selected CLI must support `build --manifest` and `manifest inspect/push`
for the release image targets.

Set `RUST_BUILDER_IMAGE` to the Rust builder image used by the release
Dockerfiles. Do not use `rust:latest` for release validation because it can drift
from the image build inputs.

Container CLI prerequisites:

```shell
make release-images-check-cli
```

If the amd64 Rust smoke test fails under emulation, do not publish a stable
arm64-only tag by default. Use a Podman farm or remote Podman connection with a
native amd64 builder, or document the Docker release as blocked. If the release
owner explicitly narrows the Docker scope to an arm64-first publish, push only
the versioned arm64 tags, leave `latest` untouched, and record the missing
amd64/multi-arch artifacts as a release gap. Set
`RELEASE_IMAGE_PLATFORMS=linux/arm64` before running the image Make targets for
that scoped build.

Build both platforms into local manifests, inspect them, and push the manifest
lists:

```shell
make release-images
```

To split the image path into smaller steps, run:

```shell
make release-images-build
make release-images-inspect
make release-images-push
```

Verify the registry exposes every platform listed in `RELEASE_IMAGE_PLATFORMS`
for all four images:

```shell
make release-images-verify
```

After the image tags and PyPI package are published, run the Docker Compose
release smoke check. It pulls the target image tag, starts a compose cluster, and
runs `python -m flamepy.runner.e2e --tasks 1 --json` from a clean Python image
that installs `flamepy==${PYTHON_VERSION}` from PyPI instead of using the SDK
preinstalled in Flame images:

```shell
RELEASE_SANITY_LOCAL_CHECKS=0 \
RELEASE_SANITY_PACKAGE_CHECKS=0 \
RELEASE_SANITY_REMOTE_CHECKS=1 \
RELEASE_SANITY_COMPOSE_E2E=1 \
make release-sanity
```

Set `RELEASE_SANITY_COMPOSE_DOWN=0` only when you need to inspect the compose
cluster after a failed run.

The compose smoke uses the TLS settings in `ci/flame-cluster.yaml` and
`ci/flame.yaml`. If `ci/certs` does not already contain release-test
certificates, generate them before running the compose sanity check:

```shell
ci/generate-certs.sh --output ci/certs \
  --san-list localhost,127.0.0.1,flame-session-manager,flame-object-cache \
  --ip-range 172.20.0.0/24
```

For an explicitly scoped arm64-first Docker release, keep the compose smoke
versioned-tag-only and set the expected platform list:

```shell
RELEASE_SANITY_EXPECTED_PLATFORMS=linux/arm64 \
RELEASE_SANITY_LOCAL_CHECKS=0 \
RELEASE_SANITY_PACKAGE_CHECKS=0 \
RELEASE_SANITY_REMOTE_CHECKS=1 \
RELEASE_SANITY_COMPOSE_E2E=1 \
make release-sanity
```

If Docker Hub times out while pulling base images, retry the base image pull for
the affected platform before rebuilding. Use the matching tool for the selected
build path:

```shell
make release-images-pull-bases
```

## Kubernetes And Helm Verification

The Helm chart defaults to:

- `global.imageRegistry: xflops`
- `global.imageTag: latest`
- component repositories matching the Docker Hub names above

For release validation, install with the versioned tag:

```shell
helm template flame charts/flame --set global.imageTag="${DOCKER_TAG}"
helm install flame charts/flame \
  --namespace flame --create-namespace \
  --set global.imageTag="${DOCKER_TAG}"
helm test flame --namespace flame
```

For Kind-based validation, load or pull the four versioned images and run:

```shell
IMAGE_REGISTRY="${IMAGE_REGISTRY}" IMAGE_TAG="${DOCKER_TAG}" ci/k8s/e2e.sh
```

Capture the `helm test`, `flmctl list`, `flmctl list -n`, `flmping`, and Python
Pi example output when reporting the result.

## Git Tag And GitHub Release

Tag only the final verified release commit:

```shell
git status --short
git tag -a "${RELEASE_TAG}" -m "Release ${RELEASE_TAG}"
git push upstream "${RELEASE_TAG}"
```

Create the GitHub release after package and image URLs are verified:

```shell
gh release create "${RELEASE_TAG}" \
  --repo xflops/flame \
  --target "${RELEASE_BRANCH}" \
  --title "${RELEASE_TAG}" \
  --notes-file "${RELEASE_NOTES_FILE}" \
  --prerelease
```

Omit `--prerelease` for stable releases.

## Final Verification

Before declaring the release complete, record:

- Git tag and commit SHA.
- GitHub release URL.
- PyPI `flamepy` version URL and uploaded files.
- crates.io URLs for `stdng`, `flame-rs-macros`, and `flame-rs`.
- Docker Hub tag URLs and manifest digests for all four images.
- CI run URLs for the final release commit.
- Helm/Kubernetes smoke-test output or the reason it was not run locally.

The release is not complete until every intended artifact has a verified remote
URL or the missing artifact is explicitly documented as blocked.

## Failure Rules

- Do not overwrite Cargo or PyPI versions. Publish a new release candidate.
- Do not force-push public release tags without release-owner approval.
- Do not move `latest` for release candidates.
- Do not reduce E2E coverage to make a release pass.
- If a release branch needs a fix, land the source change on `main` first when
  practical, then create a dedicated cherry-pick PR to the release branch.
