# Salty Gull

Over-the-air updates for Flutter and desktop apps, from WeirdBrains. Salty Gull replaces Shorebird for
WeirdBrains apps (Morph first). Updates are signed by the publishing org, verified on the device, and never
installed without the user's consent.

This is the G3 MVP in progress: a single-org self-hosted server, the publisher CLI and the desktop
updater library. Generic update logic (roots, manifests, verification, anti-rollback, targeting, rollouts,
consent, boot health) lives in [`weirdnet-update`](https://github.com/WeirdBrains/weirdnet-update); only Salty
Gull-specific code is here.

| Crate | What |
|---|---|
| `salty-core` | Store: verified roots and manifests (SQLite), BLAKE3-addressed blobs, device bundles |
| `salty-server` | HTTP server (axum): public reads, admin-token publishing, read-only MCP at `/mcp` |
| `saltyctl` | Keys, roots, root pins, manifests, publishing, `saltyctl mcp` (stdio bridge) |
| `salty-client` | Device updater: check, verify, consent, download, install, trial boot, rollback |

## Try it (temp dirs)

```sh
saltyctl key-new root.key                 # prints the public key; the file is mode 0600
REL=$(saltyctl key-new release.key)
PIN=$(saltyctl root-new --org wb --sign root.key --release-key "$REL" --out root.cbor)   # compile PIN into the app
saltyctl manifest --app demo --channel stable --seq 1 --version 1.0.1 \
  --platform linux --arch x86-64 --format tar-gz --signing-identity "minisign:RW..." \
  --key release.key --dir build --out m.cbor
export SALTY_TOKEN=$(salty-server --home ./srv admin-token)   # shown once; only its SHA-256 is kept
salty-server --home ./srv serve --org wb --bind 127.0.0.1:8470 &
saltyctl publish-root root.cbor --server http://127.0.0.1:8470
saltyctl publish m.cbor --dir build --server http://127.0.0.1:8470
```

## HTTP API

| Method | Path | Auth |
|---|---|---|
| GET | `/healthz` | no |
| GET | `/v1/roots` | no |
| GET | `/v1/apps/{app}/channels/{channel}/bundle` | no |
| GET | `/v1/blobs/{blake3}` | no |
| PUT | `/v1/blobs/{blake3}` | admin bearer token |
| POST | `/v1/roots` | admin bearer token |
| POST | `/v1/manifests` | admin bearer token |
| POST | `/mcp` | admin bearer token (read-only tools) |

The server checks every root (first: its own threshold; rotations: old and new thresholds), every blob hash and
every manifest (delegated release keys, validity, expiry, rising seq, files present) before storing it. Org
root and release keys never leave the publisher.

## Installs

| Format | How |
|---|---|
| tar.gz, zip, `.app` | Unpacked (regular files and directories only, no `..`) into `versions/<id>`, then a trial boot with rollback |
| exe | Copied into `versions/<id>` and made executable; trial boot |
| msi | `msiexec /i … MSIINSTALLPERUSER=1 ALLUSERS=2` (per user, no admin) |
| dmg | Opened for the user with `open` |
| deb, rpm | `pkexec apt-get install` / `pkexec dnf install`: the system asks for the password |
| FreeBSD, OpenBSD packages | `doas pkg add` / `doas pkg_add` |
| pkg.tar.zst, nix, brew, winget | Notify only: shows the upgrade command, never downloads |

Salty Gull never raises privileges itself. When an app sets a minisign publisher key, every package needs a
`.minisig` from that key, and the manifest's signing identity must match the running build.

## MCP

`saltyctl mcp --server URL` (token in `SALTY_TOKEN`) bridges stdio to the server's `/mcp`. Tools:
`list_channels`, `channel_history`, `root_info`. They are read-only, because agents never hold signing keys.

## BSD builds

`scripts/cross-bsd.sh SYSROOT_DIR [freebsd|openbsd|all]` cross-builds from Linux with clang and lld against
checksum-verified FreeBSD 14.5 and OpenBSD 8.0 sysroots. OpenBSD is a tier-3 Rust target, so it uses nightly
with `-Zbuild-std`. The binaries are built but not yet run on real BSD machines.

## Status

Done: the verified store, server, CLI, desktop updater with platform installers, minisign checks, MCP and
BSD cross-builds, with end-to-end tests. Next: the Flutter plugin, headless GUI tests, Morph integration
(see [docs/morph.md](docs/morph.md)) and running the BSD builds on real machines.

## Licence

BSD-3-Clause (see [LICENSE](LICENSE)).
