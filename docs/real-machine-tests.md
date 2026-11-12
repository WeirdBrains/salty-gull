# Real-machine tests

Run 2026-11-12 on alien-blue (headless VMs: QEMU/KVM in a container, disks under `/mnt/ext5t/scratch`,
serial console only) and the Windows VM WIN-R0LRC6FBTOM.

## Linux, FreeBSD 14.5, OpenBSD 8.0: `scripts/device-e2e.sh`

Cross-built `saltyctl`, `salty-server`, `salty-launch` and four `salty-demo` builds (1.0.0, 1.1.0, a broken
1.2.0, 1.3.0), copied to each machine. Everything runs in a temp dir against a local `salty-server`:

| Step | Linux | FreeBSD 14.5-RELEASE-p2 | OpenBSD 8.0 |
|---|---|---|---|
| Server up, root published | ok | ok | ok |
| Launcher runs the shipped 1.0.0 | ok | ok | ok (after the `current_exe` fix) |
| 1.1.0 tar.gz downloaded, verified, swapped in, trial-booted, committed | ok | ok | ok |
| Broken 1.2.0 fails twice, rolls back to 1.1.0, refused afterwards | ok | ok | ok |
| Tampered blob refused (hash mismatch) | ok | ok | ok |
| No doas rule: package update refused, nothing installed | n/a | ok | ok |
| Native package 1.3.0 via `doas pkg add` / `doas pkg_add`, trial-boot, commit, remove | n/a | ok | ok |

The VMs use a test-only doas rule (`permit nopass salty as root cmd pkg`, or `pkg_add`/`pkg_delete` on
OpenBSD) in place of a person typing their password at the doas prompt. FreeBSD's `doas` package installs a
sample `doas.conf` with `permit nopass :wheel`; the test replaced it.

Found: on OpenBSD, `std::env::current_exe` panicked on Rust 1.99 to 1.100 and nightlies before 2026-11-04
(it divided the `KERN_PROC_ARGV` buffer, which also holds the strings, by the pointer size). Rust fixed
it in rust-lang/rust#163393 (`getexecpath(3)`), so OpenBSD builds are pinned to `nightly-2026-11-12`
(1.102.0-nightly 3ef0c22cd) in `scripts/cross-bsd.sh` and the launcher's `argv[0]` workaround is gone.
Rerun 2026-11-12 with that nightly: OpenBSD and FreeBSD all ok. OpenBSD packages need `pkg_add -D unsigned` (verified by Salty Gull).

## Windows: per-user MSI (`scripts/windows-msi/test.ps1`)

MSIs built on Linux with `wixl` (msitools 0.106) from `demo.wxs`: per-user scope, LUA package,
`MajorUpgrade AllowDowngrades`. UAC is off on the VM, so its own account is always a full admin; the test
ran as a temporary standard local user (not in Administrators, medium integrity), deleted afterwards with
its profile.

| Step | Result |
|---|---|
| Control: a per-machine MSI without admin | refused (msiexec 1603, error 1925) |
| 1.0.0 per-user install | ok: `%LOCALAPPDATA%\Programs\SaltyDemo`, per-user context, nothing per machine |
| Launcher runs 1.0.0 | ok |
| 1.1.0: download, verify, `msiexec` handed off detached, trial start, commit | ok |
| Broken 1.2.0: two failed starts, launcher reinstalls the kept 1.1.0 MSI (downgrade), 1.2.0 refused | ok |
| Uninstall | ok: no product registration, files or registry keys left |

Found and fixed: msiexec rejects `/` in the package path (error 2203), which a state dir from a config
file may contain; and `installed` moved the MSI before msiexec read it (the kept copy is now made in
`install_plan` and the installer runs that copy).
