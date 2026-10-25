# rustterm

[![Build](https://github.com/anugcodes/rustterm/actions/workflows/ci.yml/badge.svg)](https://github.com/anugcodes/rustterm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)

A fast, lightweight, native Linux alternative to MobaXterm. SSH terminal tabs with an SFTP side panel that follows your session. Written in Rust.

> **Status:** working terminal with tabs. Colours, scrollback, copy/paste, mouse
> reporting and TOFU host-key verification are done. Saved sessions + keyring and
> the SFTP side panel are next.

## Screenshots

_(coming soon)_

## Features

- [x] SSH terminal with 256-colour support, background colours and reverse video
- [x] Multiple sessions in tabs
- [x] Scrollback (mouse wheel + `Shift+PageUp` / `PageDown` / `Home` / `End`)
- [x] Copy/paste: mouse selection, `Ctrl+Shift+C` / `Ctrl+Shift+V`, right-click menu
- [x] Mouse reporting for full-screen apps (htop, vim, tmux, mc)
- [x] Application cursor keys (DECCKM) so ncurses programs get real arrow keys
- [x] Host-key verification (TOFU) against `~/.ssh/known_hosts`
- [ ] Saved sessions + system keyring
- [ ] SFTP side panel following the active session
- [ ] Upload/download with drag and drop
- [ ] Edit-and-auto-upload remote files
- [ ] X11 forwarding / VNC tunnel for GUI tools (Verdi, DVE)

## Stack

| Concern | Crate |
|---|---|
| UI | `egui` + `eframe` |
| SSH | `russh` (pure Rust) |
| Terminal | `alacritty_terminal` |
| Async | `tokio` |

## Build

```bash
sudo apt install libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev
cargo build --release
```

Run:

```
cargo run --release
```

Enter a host, port, username, and password, then click **Connect**.

## Documentation

- [`USAGE.md`](USAGE.md) — how each feature works, plus a manual test checklist.
- [`handoff.md`](handoff.md) — architecture, status and the development roadmap.

## Roadmap

See the project brief. MVP target: terminal + tabs + saved sessions + SFTP panel.

## License

MIT
