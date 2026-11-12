# Changelog

All notable changes to this project are documented here. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning: [SemVer](https://semver.org/).

## [Unreleased]

### Added

- Single-org update server with verified roots, manifests and content-addressed blobs.
- `saltyctl`: keys, org roots and pins, signed full-package manifests, publishing.
- Desktop updater core: verify, consent, download, staged trial boot and automatic rollback.
- Platform installers: app-directory swap for tar.gz, zip, `.app` and exe; per-user MSI; dmg; deb and rpm
  through `pkexec`; BSD packages through `doas`; notify-only pkg.tar.zst, nix, brew and winget.
- minisign verification of desktop packages and signing-identity checks.
- Read-only MCP tools at `POST /mcp` and `saltyctl mcp`.
- `scripts/cross-bsd.sh` for FreeBSD and OpenBSD cross-builds.
- Morph integration plan (`docs/morph.md`).

### Changed

- `saltyctl` no longer depends on `salty-core`.
