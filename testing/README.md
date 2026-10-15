# Testing

| Directory | What it covers |
| --- | --- |
| [`e2e/`](e2e/README.md) | End-to-end tests (pytest) run against a Flame cluster |
| [`system/perf/`](system/perf/README.md) | Performance tests (Python benchmarks; use [`flmbench`](../flmbench/README.md) for bursts) |
| [`system/stress/`](system/stress/README.md) | Stress test: ladder of heavy bursts, then a recovery check |
| [`system/longevity/`](system/longevity/README.md) | Longevity test: sustained load for hours |

## flame-test image

`xflops/flame-test` is the `flame-console` image plus these suites under
`/opt/testing`, so every tool (`flmctl`, `flmbench`, `flmping`, `flmexec`, `flamepy`,
`pytest`, `jq`) is ready to use. Run a suite with `flame-test`:

```sh
flame-test e2e tests/test_session.py
flame-test perf core
PROFILE=smoke flame-test stress
DURATION=8h flame-test longevity
flame-test bench -s 4 -t 10000 -n 3
```

It is built and published with the other images (`docker/Dockerfile.testing`, a
`flame-test` target in `ci/release/image-bake.hcl`). Bake builds it from the
`flame-console` target of the same invocation, so it never depends on a console image
that is already published.
