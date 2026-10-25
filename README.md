# rustterm

[![Build](https://github.com/anugcodes/rustterm/actions/workflows/ci.yml/badge.svg)](https://github.com/anugcodes/rustterm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)

A fast, lightweight, native Linux alternative to MobaXterm. SSH terminal tabs with an SFTP side panel that follows your session. Written in Rust.

> **Status:** working terminal with tabs. Colours, scrollback, copy/paste, mouse
> reporting, TOFU host-key verification, saved sessions, keyring passwords,
> public-key/agent auth and the SFTP side panel (sortable table, reconnect,
> rename, delete, copy/paste, hidden files and an **Open with…** editor picker)
> are done. The UI has a shared, persisted theme system. General upload/download
> transfers are next; the panel does not yet follow shell `cd`.

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
- [x] Saved sessions + optional system-keyring passwords
- [x] Public-key and ssh-agent authentication
- [x] SFTP side panel per session: **Name / Owner / Modified / Size** table
- [x] SFTP message status bar with history and per-operation progress
- [x] SFTP reconnect without dropping the terminal
- [x] SFTP rename, delete, new folder, copy/paste and file operations
- [x] Open remote files in a local editor via an **Open with…** picker
      (`$VISUAL`/`$EDITOR`, Zed, gvim, VS Code, desktop default, custom command)
- [x] Centred New Session page with saved-profile management
- [x] Global UI themes (Midnight / Graphite) with the choice persisted
- [ ] SFTP panel follows the shell's working directory
- [ ] Upload/download with drag and drop
- [ ] Edit-and-auto-upload remote files
- [ ] X11 forwarding / VNC tunnel for GUI tools (Verdi, DVE)

## Stack

| Concern | Crate |
|---|---|
| UI | `egui` + `eframe` + `egui_extras` (tables) |
| SSH | `russh` (pure Rust) |
| SFTP | `russh-sftp` |
| Terminal | `alacritty_terminal` |
| Async | `tokio` |
| Timestamps | `chrono` |

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
