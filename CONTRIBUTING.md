# Contributing

Thanks for helping build rustterm. This is an early-stage project, so the most valuable contributions are bug reports and small, focused PRs.

## Development setup

1. Install Rust (1.75+): https://rustup.rs
2. Install Linux GUI deps:
   ```bash
   sudo apt install libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev
```

3. Build and run:

```
cargo run
```

## Pull requests

- Keep PRs small and scoped to one feature or fix.
- Run `cargo fmt` and `cargo clippy -- -D warnings` before submitting.
- Add a short description of what changed and why.

## Code style

- Prefer plain, explicit Rust over clever abstractions.
- Reuse crates for hard problems (terminal emulation, SSH). Don't reinvent them.
- Async code uses `tokio`. UI code runs on the main thread; SSH/terminal state is driven from a background thread and polled.

## Roadmap priorities

1. Tabs (multiple sessions)
2. Session persistence + keyring
3. SFTP panel
4. Transfers
5. Edit-and-auto-upload

If you want to work on one of these, open an issue first so we don't duplicate effort.

</BDS:create_file>
<BDS:create_file fileName=".gitignore">

```text
/target
**/*.rs.bk
Cargo.lock
.idea/
.vscode/
*.swp

