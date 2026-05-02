# @relayburn/ingest

All notable changes to this package will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- New package extracted from `@relayburn/cli`. Owns Claude/Codex/OpenCode session-store discovery, incremental parse-and-append orchestration, pending-stamp resolution, content-gap warning state, and the polling watch-loop primitive. CLI and SDK now consume these operations from `@relayburn/ingest` instead of the CLI internals.
