# Changelog

All notable changes to `relayburn`.

## [Unreleased]

- Set `RELAYBURN_INSTALL_CHANNEL=npm` when launching the binary so `burn update` upgrades via `npm install -g relayburn@latest` instead of guessing the install channel.

## [2.4.0] - 2026-05-08

### Removed

- Removed the `burn run` launcher wrapper from the CLI surface. Launchers
  should write attribution with `writePendingStamp()` and ingest through
  `burn ingest` / SDK `ingest()`.
- Removed the fallback to the old TypeScript `@relayburn/cli`; `relayburn`
  now resolves only the Rust prebuilt platform packages.

## [2.0.0] - 2026-05-07

### Changed

- `relayburn` now prefers the prebuilt Rust `burn` binary from per-platform `@relayburn/cli-<platform>` packages, with `@relayburn/cli` kept as a fallback for unsupported or missing native packages.

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
