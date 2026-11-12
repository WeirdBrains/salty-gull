# Changelog

All notable changes to this project are documented here. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning: [SemVer](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-11-12

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
- `salty-launch`: launcher for portable builds and MSI installs (trial boot, rollback, MSI reinstall).
- `salty-client`: `begin_boot` for launchers, `Boot` (build dir, trial, rollback, reinstall), `installed`
  for installer formats, kept MSI packages for rollback, a build `Target` check, `Command::spawn`.
- `scripts/device-e2e.sh` and `scripts/windows-msi/` real-machine tests; `salty-demo` test app.

### Changed

- `saltyctl` no longer depends on `salty-core`.
- `salty-core`'s SQLite store is behind the default `store` feature; `salty-client` builds without SQLite.
- After a commit, later starts keep running the committed build (it was the shipped build before).
- OpenBSD packages install with `pkg_add -D unsigned` (already verified by Salty Gull).
- MSI commands add `/norestart`.

### Fixed

- msiexec failed with error 2203 when the state directory path had `/` separators on Windows.
- The MSI handed to the installer was moved by `installed` before msiexec could read it.
- OpenBSD: `std::env::current_exe` panics on current Rust nightlies; the launcher resolves its own path
  from `argv[0]` there.
