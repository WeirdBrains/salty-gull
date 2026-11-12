# Per-user MSI test

1. Build `salty-launch.exe` and `salty-demo.exe` for 1.0.0, 1.1.0 and a broken 1.2.0
   (`SALTY_DEMO_VERSION`, `SALTY_DEMO_BROKEN=1`), for example with `cargo zigbuild --target x86_64-pc-windows-gnu`.
2. For each version put `SaltyDemo.exe` (the launcher), `salty-launch.json`, `app/salty-demo.exe` and
   `app/salty-demo.json` under `build/`, set `@VERSION@` in `demo.wxs` and run `wixl -a x64 -o demo-X.msi demo.wxs`.
   Also build a per-machine variant (`InstallScope="perMachine"`, `ProgramFiles64Folder`) as the control.
3. Make keys and a root with `saltyctl`, put the pin and `"server": "http://127.0.0.1:18471"`,
   `"channel": "msi"`, `"format": "msi"`, `"state_dir": "${TEMP}/salty-msi-test/state"` in the configs, and
   build manifests `m1.cbor` (1.1.0) and `m2.cbor` (1.2.0) over `pkg-1.1.0/SaltyDemo.msi` and `pkg-1.2.0/`.
4. Copy everything with `salty-server.exe` and `saltyctl.exe` to `C:\SaltyMsiTest` and run `test.ps1` as a
   standard (non-admin) user. Results go to `out\result.txt`.
