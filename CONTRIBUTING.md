# Contributing to Flame

Thank you for your interest in Flame. Contributions of all kinds are welcome:
bug reports, feature requests, documentation, and code.

Contributions are made under the [Apache License 2.0](LICENSE), the license of
the project.

## Ways to contribute

- **Report a bug** or **request an enhancement** in the
  [issue tracker](https://github.com/xflops/flame/issues). Pick the bug report
  or feature request template.
- **Ask a question or discuss an idea** in
  [GitHub Discussions](https://github.com/xflops/flame/discussions).
- **Report a security vulnerability** privately; see [SECURITY.md](SECURITY.md).
  Do not open a public issue for it.
- **Send a pull request** for a fix, a feature or documentation.

### Reporting a bug

Search the existing issues first. A good report has the version of Flame
(`flmctl --version`, or the image tag), the operating system and how Flame was
installed, the steps to reproduce, what you expected, and what happened,
including the relevant logs (`flmctl list -s`, `flmctl list -e`, and the logs of
the session manager and the executor manager). The bug report template asks
for these. A maintainer will respond on the issue. Reports are public, which
is why vulnerabilities go through [SECURITY.md](SECURITY.md) instead.

### Requesting an enhancement

Describe the problem you want to solve before the solution. Larger changes
(a new scheduling policy, a protocol change, a new component) start with a
design document in [`docs/designs`](docs/designs), following the `RFE<issue>`
pattern of the existing ones, so that the design is reviewed before the code.

## Development setup

The [README](README.md) has the quick start, and
[Local Development](docs/tutorials/local-development.md) sets up a
single-node cluster from the source tree. You need the Rust toolchain
(`rust-toolchain.toml` pins it), `protoc`, and, for the Python SDK, Python and
[`uv`](https://docs.astral.sh/uv/).

```shell
make update_protos                 # copy the protobuf files into the SDKs
cargo build --workspace --exclude flame-rs
```

## Coding standards

- **Rust:** formatted with `cargo fmt` (see `rustfmt.toml`) and free of
  `cargo clippy` warnings; CI runs clippy with `-D warnings`.
- **Python:** formatted and linted with `ruff` (see `sdk/python/pyproject.toml`).
- Match the style of the code around your change: naming, comment density and
  idioms. Keep a change small and focused; do not mix a refactoring with a fix.
- New source files carry the Apache license header like the existing ones.
- Update the documentation (`docs/`, a crate's README, doc comments) together
  with the behavior that it describes.

Run the same checks as CI before you send a pull request:

```shell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --exclude flame-rs

cd sdk/python
uv run --extra dev ruff format --check src tests
uv run --extra dev ruff check src tests
uv run --extra dev --with numpy pytest tests
```

## Tests: the test policy

**A change that adds or changes functionality, or fixes a bug, must come with
tests.** A bug fix includes a test that fails without the fix. A pull request
without tests needs a reason in its description (for example a documentation
change, or behavior that only a cluster can show), and a reviewer may ask for
the test anyway.

- Rust: unit tests next to the code (`#[cfg(test)] mod tests`) and the
  integration tests of a crate.
- Python SDK: `sdk/python/tests`.
- End-to-end and system tests, which run against a cluster: `testing/e2e` and
  `testing/system` (see their README files).

The tests run automatically on every pull request (see
`.github/workflows`), on the host shim, the CRI shim and Kubernetes. All the
checks must pass before a pull request is merged.

## Sending a pull request

1. Fork the repository and create a branch from `main`.
2. Make the change, with its tests and documentation.
3. Run the checks above.
4. Open a pull request against `main` and fill in the template. Link the issue
   that it fixes (`Fixes #123`).
5. A maintainer reviews it. Address the comments by pushing to the same branch;
   the pull request is squash-merged, so its history does not need to be tidy.

Use a short commit subject with a conventional prefix, as in the history:
`feat:`, `fix:`, `docs:`, `test:`, `chore:`, `refactor:`, `perf:` or `ci:`.

Changes to a release branch (`release-0.7`, ...) are cherry-picks of changes
that were merged to `main` first; see the [release process](docs/release-process.md).
