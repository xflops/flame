# Changelog

All notable changes to the Object Cache feature (RFE318) will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Design for issue #318 item 3: native DataSet/DataFrame cache payloads stored as Arrow schemas and record batches instead of opaque binary rows.
- Native Arrow table payload support in object-cache Flight and disk storage paths.
- FlamePy native tabular payload classification for PyArrow tables/batches plus optional pandas/polars DataFrames.
- Unit and E2E coverage for native Arrow cache payloads.
- `patch` operation support in `ObjectCache` and `FlightCacheServer` (PR #6)
- `patch_object` function in Python SDK (PR #6)
- Append-only semantics for object updates (PR #6)
- `deltas` field in `Object` struct to support incremental updates (PR #6)
- `MAX_DELTAS_PER_OBJECT` (1000) limit to prevent unbounded delta growth
- `deserializer` parameter in `get_object()` to support custom delta merging

### Changed
- Updated `Object` struct to include `deltas` vector (PR #6)
