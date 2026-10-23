# rustterm

[![Build](https://github.com/YOUR_USERNAME/rustterm/actions/workflows/ci.yml/badge.svg)](https://github.com/YOUR_USERNAME/rustterm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)

A fast, lightweight, native Linux alternative to MobaXterm. SSH terminal tabs with an SFTP side panel that follows your session. Written in Rust.

> **Status:** early scaffold. Terminal rendering and a single SSH connection work. Tabs and SFTP are next.

## Screenshots

_(coming soon)_

## Features

- [x] SSH terminal with colors (via `alacritty_terminal`)
- [ ] Multiple sessions in tabs
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

## Roadmap

See the project brief. MVP target: terminal + tabs + saved sessions + SFTP panel.

## License

MIT

</BDS:create_file>
<BDS:create_file fileName="LICENSE">

```text
MIT License

Copyright (c) 2026 rustterm contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

