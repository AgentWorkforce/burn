# Changelog

All notable changes to `relayburn`.

## [Unreleased]

### Changed

- `relayburn` now installs the prebuilt Rust `burn` binary via per-platform `@relayburn/cli-<platform>` packages instead of dispatching through the TS `@relayburn/cli`. The umbrella declares the platform packages as `optionalDependencies`; the `burn` shim resolves and execs the matching binary.

## [1.6.2] - 2026-05-02

### Changed

- Bump package versions to 1.6.1

## [1.5.0] - 2026-05-02

### Changed

- README examples now point to `burn hotspots` instead of the removed `burn budget` command.

## [1.0.0] - 2026-04-29

### Fixed

- Published `relayburn` README examples now use `burn budget --watch` instead of the removed `burn limits` command.

## [0.43.0] - 2026-04-29

### Changed

- Tighten changelog entries

## [0.35.0] - 2026-04-28

### Added

- Initial release. This package installs the same `burn` command as `@relayburn/cli` under the unscoped `relayburn` name.
