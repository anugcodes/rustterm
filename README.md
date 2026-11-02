# rustterm

[![Build](https://github.com/anugcodes/rustterm/actions/workflows/ci.yml/badge.svg)](https://github.com/anugcodes/rustterm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)

A fast, lightweight, native Linux alternative to MobaXterm. SSH terminal tabs with an SFTP side panel that follows your session. Written in Rust.

> **Status:** feature-complete MVP. SSH terminal tabs with 256-colour output,
> scrollback, copy/paste, mouse reporting, TOFU host keys, saved profiles,
> keyring/key/agent auth and an SFTP side panel are done. The panel follows the
> shell's `cd`, supports recursive drag-and-drop upload/download with a queue and
> conflict prompts, and edit-and-auto-upload survives reconnects. X11 forwarding
> and a VNC tunnel cover remote GUI tools. Remaining work, including the planned
> module restructuring, is tracked in `handoff.md`.

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
- [x] Saved profiles + optional system-keyring passwords
- [x] Public-key and ssh-agent authentication
- [x] SFTP side panel per session: **Name / Owner / Modified / Size** table with
      header-only column resizing
- [x] SFTP message status bar with history and per-operation progress
- [x] SFTP reconnect without dropping the terminal
- [x] SFTP rename, delete, new folder, copy/paste and file operations
- [x] Open remote files in a local editor via an **Open with…** picker
      (`$VISUAL`/`$EDITOR`, Zed, gvim, VS Code, desktop default, custom command)
- [x] Compact New Session page: one connection card with an optional
      **Save as profile** field and a **Load / delete** saved-profile table
- [x] Flat, boxy UI (square corners, 1 px borders, no shadows or hover glow)
- [x] Global UI themes (Midnight / Graphite) with the choice persisted
- [x] SFTP panel follows the shell's working directory
- [x] Upload/download with drag and drop
- [x] Edit-and-auto-upload remote files
- [x] X11 forwarding / VNC tunnel for GUI tools (Verdi, DVE)

## Stack

| Concern | Crate |
|---|---|
| UI | `egui` + `eframe` (hand-rolled SFTP table) |
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
- [`TESTING.md`](TESTING.md) — the manual smoke and full test procedures.
- [`handoff.md`](handoff.md) — architecture, status and the development roadmap.

## Roadmap

See the project brief. MVP target: terminal + tabs + saved sessions + SFTP panel.

## License

MIT
