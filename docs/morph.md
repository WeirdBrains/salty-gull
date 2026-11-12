# Morph integration (prep)

Status: plan only. Nothing here changes Morph's repo; that needs Michael's go-ahead. Morph's side is
`ARCHITECTURE.md` section 9a in `WeirdBrains/morph`: signed full updates for desktop and sideloaded builds,
never forced, opt-in per update or for all updates, F-Droid off until the user opts in, store builds take
no full updates.

## Shape

Morph links `salty-client` from its Rust core and exposes it to Dart through `morph-bridge`
(`flutter_rust_bridge`). Dart owns the consent UI; Rust owns verification, downloads and installs.

| Bridge call | `salty-client` | When |
|---|---|---|
| `update_boot()` | `Updater::begin_boot` | first thing at startup; `RollBack` means the last update failed twice |
| `update_ready()` | `Updater::mark_ready` | after the first frame and a healthy core start |
| `update_check()` | `Updater::check` | on start and every few hours while in the foreground |
| `update_accept(id)` / `update_decline(id)` | `accept` / `decline` | from the consent prompt |
| `update_download(id)` | `download` | after consent (or at once in "all updates" mode) |
| `update_install(id)` | `install_plan` then `Plan::run` or swap | on "restart to update" |
| `update_mode(mode)` | `set_mode` | settings: Ask (default), All updates, Off |

`check` returns the notice (version, notes, size). Notify-only formats (pkg.tar.zst, nix, brew, winget)
only show the command, for example `yay -Syu morph`; Morph never downloads them.

## Config compiled into each build

- `server`: `https://updates.weirdbrains.example` (placeholder; no server is deployed).
- `org` and the **root pin** from `saltyctl root-pin` for the WeirdBrains org root.
- `app = "morph"`, `channel` = `stable` (plus `beta` in beta builds).
- `platform`, `arch`, `format`: set by the packaging job (AppImage/tar.gz, deb, rpm, msi, dmg, zip `.app`).
- `signing_identity`: the platform signing identity of this build (minisign key id on Linux and BSD,
  Authenticode subject on Windows, Developer ID team on macOS). A mismatch refuses the update.
- `minisign_key`: the WeirdBrains minisign public key for Linux and BSD packages.
- `home`: `dirs::data_dir()/morph/updates`, created 0700. Tests use temp dirs only.

## Installs per format

- **tar.gz / zip / `.app` (swap):** unpacked into `versions/<id>`; a small `morph` launcher starts the
  newest healthy version and falls back after two failed boots. Needs a launcher in Morph's packaging.
- **exe / msi:** per-user install through `msiexec` (no admin); **dmg:** opened for the user.
- **deb / rpm:** `pkexec apt-get install` / `pkexec dnf install`, so the system asks for the password.
- **FreeBSD / OpenBSD:** `doas pkg add` / `doas pkg_add`.

## Flavours

- **Desktop and sideloaded:** as above.
- **F-Droid:** mode starts at Off; the first-run prompt uses F-Droid wording; no full self-update.
- **App Store / Play:** no `salty-client` full updates; asset and WASM bundles come later (Phase 2).

## Open decisions

1. How Morph depends on `salty-client`: a pinned git rev of `WeirdBrains/salty-gull` (nothing published),
   or publishing `salty-client` to crates.io (a Salty Gull release, so it needs a yes).
2. Whether Morph ships the swap launcher (portable tar.gz/AppImage) or only package-manager formats.
3. The update server's host name and where it runs (no infrastructure is bought or deployed).
4. Updates over Weirdnet relays (section 9a) are later; HTTPS is the first transport.
