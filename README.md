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
| `salty-server` | HTTP server (axum): public reads, admin-token publishing |
| `saltyctl` | Keys, roots, root pins, manifests, publishing |
| `salty-client` | Device updater: check, verify, consent, download, stage, trial boot, rollback |

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

The server checks every root (first: its own threshold; rotations: old and new thresholds), every blob hash and
every manifest (delegated release keys, validity, expiry, rising seq, files present) before storing it. Org
root and release keys never leave the publisher.

## Status

Done: the verified store, server, CLI and desktop updater core with end-to-end tests. Next for G3: platform
installers (app-directory swap for tar.gz/zip, per-user exe/MSI, `.app`), minisign verification of desktop
packages, deb/rpm through the system's approved privilege prompt, notify-only for pkg.tar.zst/nix/brew/winget,
`saltyctl mcp`, Morph desktop integration and FreeBSD/OpenBSD builds.

## Licence

BSD-3-Clause (see [LICENSE](LICENSE)).
