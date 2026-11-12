# AGENTS.md (Salty Gull)

Read and follow the collective `WeirdBrains/weirdnet-agents` AGENTS.md first (`git pull` it before work).
This file adds the Salty Gull rules.

- Generic update logic goes in `weirdnet-update`, not here. Only Salty Gull-specific code (service, billing,
  CLI, Flutter integration, installers, web UI) lives in `salty-*` crates. (2026-11-12)
- Core logic in `salty-core` / `salty-client`; `salty-server`, `saltyctl` and the Flutter plugin stay thin.
- Updates are never forced: no download or install without the user's mode or answer (`weirdnet-update`
  policy). Declining never blocks the app.
- The service never holds org root keys; it verifies what it stores.
- Never run cargo inside `~/Projects`; build with `CARGO_TARGET_DIR` under `/mnt/ext1/scratch` and clear
  build targets afterwards.
- Tests use temp dirs only; never touch a real home, config or data dir.
- Secrets (admin tokens, key seeds) are never logged or committed. Tokens are shown once at issue; only
  their SHA-256 is stored. Key files are mode 0600 and never overwritten.
- Before every commit: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features
  -- -D warnings`, `cargo test --workspace`.
- Don't deploy anything live or buy infrastructure without Michael's approval.
- Licence: BSD-3-Clause (Michael, 2026-11-12). The repo is public (`WeirdBrains/salty-gull`).
- On Linux, GUI and screenshot tests (Flutter plugin, consent UI) run in a headless compositor
  (`weston --backend=headless`, `cage`) or `xvfb-run`, never on Michael's live desktop.
- Don't release Salty Gull, deploy anything live, buy infrastructure, or change Morph's `main` without asking.
