# Handoff — rustterm (working title)

> **Status:** MVP Terminal & Tabs complete. Core rendering (F1-F5, F7-F9) is finished.
> F7 EventProxy compiles and runs; copy/paste (F14) is done.
> Next milestone is session persistence and SFTP. Read section 12 before writing any code.

Last updated: 2026-10-25
Repo: `~/Desktop/VsTerm` (local) — remote may be `rustterm` on GitHub
Owner: anurag (Linux Mint 22)

---

## 1. What this project is

A fast, native, lightweight **MobaXterm alternative for Linux**, written in Rust.

MobaXterm is Windows-only and its killer feature is not the SSH terminal — it's that
an **SFTP file browser is auto-attached to every session and follows the remote path**
as you `cd` around. Linux has no good equivalent: Tabby is Electron-heavy, Termius is
paid, WindTerm is closed source.

**Scope discipline:** SSH terminal + tabs + SFTP side panel. Nothing else until those
are done. No reverse engineering of MobaXterm — implement the concept from scratch on
open protocols.

### Eventual feature set (from the project brief)

| # | Feature | Status |
|---|---|---|
| 1 | SSH terminal with colors, resize, copy/paste | **Partial** — colors, resize, scrollback, bg colors yes. Copy/paste no. |
| 2 | Multiple SSH sessions in tabs | **Done** |
| 3 | Saved sessions, key auth, keyring passwords | Not started |
| 4 | SFTP side panel following the active session's path | Not started |
| 5 | Upload/download, drag & drop, progress | Not started |
| 6 | Open remote file in local editor, auto-upload on save | Not started |
| 7 | GUI tools via X11 forwarding (`ssh -X`) or VNC-over-tunnel | Not started |

---

## 2. Stack and why

| Concern | Crate | Version | Notes |
|---|---|---|---|
| UI | `eframe` / `egui` | 0.29 | Fastest path to a desktop tool. |
| SSH | `russh` | 0.45 | Pure Rust, no libssh2 C dependency. |
| Terminal emulation | `alacritty_terminal` | 0.24 | **Do not write a custom VT parser.** |
| Async | `tokio` | 1.x | One runtime per SSH session thread. |
| Error handling | `anyhow` | 1 | |
| Async trait | `async-trait` | 0.1 | Required for `russh::client::Handler`. |
| Logging | `log` + `env_logger` | 0.11 | `RUST_LOG=info cargo run` |

`eframe` uses `default-features = false, features = ["glow", "default_fonts"]`.

**Not yet added but will be needed:** `russh-sftp` (SFTP), `keyring` (secrets),
`notify` (file watching for auto-upload), `rfd` (native file dialogs), `dirs` or
`directories` (config/temp paths).

---

## 3. Repo layout

```

Cargo.toml                  # eframe/egui 0.29, russh 0.45, alacritty_terminal 0.24
Cargo.lock                  # COMMITTED (binary crate — reproducible builds)
README.md                   # badges use YOUR_USERNAME placeholder — fix before push
LICENSE                     # MIT
CONTRIBUTING.md
USAGE.md                    # feature usage + manual GUI test checklist
handoff.md                  # this file
.gitignore
.github/workflows/ci.yml    # fmt + clippy -D warnings + build, ubuntu-latest
scripts/setup-github.sh     # repo creation + push WITHOUT the GitHub CLI
assets/README.md            # where to drop JetBrainsMono-Regular.ttf
src/
main.rs                   # font loading, eframe::run_native
app.rs                    # RustTermApp: tabs, connect form, terminal UI, input dispatch
ssh.rs                    # SshSession: russh client, host-key prompt, thread + tokio
terminal.rs               # TerminalView: Term<EventProxy> + egui rendering + layout
input.rs                  # egui events -> pty bytes (xterm encoding)

```

`assets/JetBrainsMono-Regular.ttf` is **optional**. It is loaded at runtime with
`std::fs::read`; if absent, egui's built-in monospace is used and a warning is logged.
Do not switch this back to `include_bytes!`.

---

## 4. Build and run

### System dependencies (Ubuntu / Mint / Debian)

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config curl \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxkbcommon-dev libssl-dev \
  libfreetype6-dev libfontconfig1-dev \
  libgtk-3-dev
```

### Run

```
cargo build          # first build ~3-8 min
cargo run
RUST_LOG=info cargo run   # see font-loading and session logs
```

CI runs `cargo clippy -- -D warnings`. Fix all warnings before pushing.

---

## 5. How to test (local SSH server)

```
sudo apt install -y openssh-server
# Mint 22 uses socket activation:
systemctl is-active ssh.socket

ssh localhost        # verify with the system client first
```

Create a disposable test user with a **real shell** — `request_shell` execs the
user's login shell, so a `nologin` user connects and immediately closes:

```
sudo useradd -m -s /bin/bash sshuser
sudo passwd sshuser
```

Connect in rustterm with host `localhost`, port `22`, user `sshuser`.

---

## 6. Architecture / data flow

```
OS keyboard ──► egui ──► app.rs::terminal_ui
                            │  event_to_bytes()  (input.rs)
                            ▼
                    SshSession::write()  ── mpsc::UnboundedSender<Vec<u8>>
                            │
                            ▼
              [background thread + tokio runtime]
                    run_session(): tokio::select! over
                      - input_rx.recv()      -> channel.data()
                      - resize_rx.recv()     -> channel.window_change()
                      - disconnect_rx.recv() -> break
                      - channel.wait()       -> output_tx.send()
                            │
                            ▼
                    std::sync::mpsc::Receiver<Vec<u8>>
                            │
              app.rs drains each frame -> TerminalView::feed()
                            │
                            ▼
              alacritty_terminal vte::Processor -> Term<EventProxy> grid
                            │
                            ▼
              TerminalView::ui() paints backgrounds, then one
              LayoutJob galley per row (memoized by egui).
```

Key design points:

- **One OS thread + one current-thread tokio runtime per SSH session.** The egui UI
thread never blocks on network I/O.
- **Two different channel types on purpose:** `tokio::sync::mpsc` for UI→SSH (async
side), `std::sync::mpsc` for SSH→UI (drained with `try_recv`, never blocks).
- **Repaint gating:** `app.rs` only requests a repaint when output arrives, a
prompt is pending, or during a 3-frame tail (`POST_ACTIVITY_FRAMES`). Idle CPU is
~0%.
- **EventProxy holds `input_tx`.** It answers DA1/DA2/DSR queries by writing the
reply bytes back to the PTY. It also handles `Title`, `Bell`, `TextAreaSizeRequest`,
and `ColorRequest`.

---

## 7. What works today (verified)

- **Tabs**: Multiple concurrent SSH sessions. `＋ New Session` tab is always present
and not closable. Closing a tab disconnects its SSH cleanly.
- **Host key verification (F3)**: `known_hosts` checking via `russh-keys`. TOFU
prompt for unknown hosts; *hard failure* (no bypass) for changed keys. Handles
hashed `known_hosts` and missing `~/.ssh`.
- **Synchronous connect (F1)**: `SshSession::connect()` blocks through TCP + host
key + auth. The connect form correctly reports failures (dead `Ok(Err)` branch is
gone).
- **Explicit disconnect (F2)**: Tab close and the Disconnect button both signal
`disconnect_tx` so the SSH connection does not leak.
- **Real font metrics (F5)**: Measures the advance width by shaping `"MMMMMMMMMM"`
and dividing by 10, so the cursor aligns exactly with the shaped text.
- **Batched rendering (F4)**: One `LayoutJob` per row (not per character).
Background colours are painted as contiguous rectangular spans.
- **Scrollback (F8)**: Mouse wheel and `Shift+PgUp/PgDn/Home/End` scroll into
history. Alt-screen programs (`vim`, `htop`) ignore it, as intended.
- **Background colours (F9)**: `cell.bg` and `Flags::INVERSE` are painted.
`htop` selection highlights and `tmux` status bars colour correctly.
- **EventProxy (F7)**: `PtyWrite` replies are sent back to the PTY, so `vim`, `tmux`,
and `tput` do not stall on capability queries. `Title` updates the tab label.
`Bell` logs.
- **Resize forwarding**: `take_resize()` reports grid changes to the remote PTY.

---

## 8. Known flaws and technical debt

Severity: **A** = must fix before MVP, **B** = should fix, **C** = polish/later.

### Resolved

**F1** — Synchronous connect ✅
**F2** — Explicit disconnect ✅
**F3** — Host key verification ✅
**F4** — Batched rendering ✅
**F5** — Real font metrics ✅
**F7** — EventProxy capability queries ✅ (compiles + app launches; needs a visual check that `htop`/`tmux` don't stall)
**F8** — Scrollback ✅
**F9** — Cell backgrounds / inverse ✅ Background runs are now merged by a unit-tested helper (`bg_runs`); the previous inline version extended the *previous* run, so backgrounds bled across the row (nano/htop). Reverse-video text is no longer white-on-white (`NamedColor::Background` now maps to the dark default, not the same grey as `Foreground`).
**F14** — Copy/paste ✅ Keyboard (`Ctrl+Shift+C` copy / `Ctrl+Shift+V` paste) **and** a right-click context menu (Copy / Paste / Select All / Clear selection). Selection + copy also work inside alt-screen apps (`vim`/`htop`/`tmux`). Bare `Ctrl+C` still sends `ETX`. Details + test checklist in `USAGE.md` §4.
**F17** — Mouse reporting ✅ `?1000`/`?1002`/`?1003` tracking and SGR `?1006` are forwarded to the remote app as xterm mouse reports, so clicks/drag/wheel work in `htop`, `vim` (`:set mouse=a`), `tmux`, `mc`, `lazygit`, … **Shift** forces local selection instead; the right button stays local for the context menu. The wheel uses button codes **64/65** (xterm adds 64 to buttons 4/5); sending bare 4/5 is decoded as a left/middle *click*. UTF-8 mouse (`?1005`) is not handled.
**F22** — Application cursor keys (DECCKM) ✅ `?1h` is honoured: cursor/Home/End are sent as SS3 (`ESC O A`) when the app enables application cursor keys, CSI (`ESC [ A`) otherwise. ncurses programs (htop, vim, mc) enable `?1h` via terminfo's `smkx` and only understand the SS3 form — sending CSI makes them see a stray Escape plus literal characters.

### Active

**F6 — `vte::Processor::advance` is one byte at a time. (B)**
Still a loop. Fine for interactive use, slow for `cat bigfile`. Requires a `vte`
upgrade which `alacritty_terminal` 0.24 does not currently allow.

**F10 — Wide characters (B)**
Spacer cells are skipped, but full CJK / emoji alignment is not verified. Double-width
characters may still break column alignment.

**F11 — 256-colour palette (C)**
Now uses the standard xterm cube (0, 95, 135, 175, 215, 255) plus the 24-step grey ramp, with full `NamedColor` coverage (dim / bright-foreground / cursor). Still does **not** read the remote's `Term::colors()` (a theme's custom palette), so app colours may not match the remote theme exactly. Low priority.

**F12 — Cursor (C)**
Still a static translucent block. No blink, no `DECSCUSR`, ignores `DECTCEM`.

**F13 — Initial PTY size (C)**
Hardcoded `80x24` until the first `ui()` frame. Produces a small reflow on connect.

**F15 — Bracketed paste mode (B)**
`?2004h` is ignored. Multi-line pastes will auto-execute.

**F16 — Per-frame `mods` (C)**
Read once per frame rather than per event.

**F18 — Tests (B)**
`terminal.rs` has unit tests for `bg_runs`, the colour mapping and the mouse encoding; `input.rs` has tests for the key encoding incl. DECCKM. **19 tests**, run with `cargo test`. Still missing: end-to-end tests for `ssh.rs`.

**F19 — Plaintext password field (A before shipping)**
No zeroize, no keyring. Must move to `keyring` + `secrecy` before release.

**F20 / F21 — Repo polish (C)**
Fix `YOUR_USERNAME` in README badges; remove the pointless `.gitignore` rule.

---

## 9. Critical egui gotchas

1. **egui steals Ctrl+C / Ctrl+X / Ctrl+V.** It emits `Event::Copy`, `Event::Cut`,
`Event::Paste` and **never delivers the raw `Event::Key`**. Handled in `input.rs`.
2. **`0x18` is not nano-specific.** Universal ASCII `CAN`. Never special-case per
program.
3. **egui turns Tab, arrows and Escape into focus navigation.** Fix:
`Memory::set_focus_lock_filter`.
4. **Do not call `request_focus()` unconditionally.** It fights the lock filter.
5. **Set the lock filter every frame**, after the focus request.
6. **`include_bytes!` is compile-time.** Use `std::fs::read` at runtime.

---

## 10. API facts (cost debugging time)

### alacritty_terminal 0.24 / vte 0.13

- `Processor::advance(&mut term, byte: u8)` — **one byte**. Loop.
- `Point { line: Line, column: Column }`. `Line + Column` is **not** implemented.
- `Term::resize(size)` takes the dimensions **by value**.
- `Dimensions` requires `total_lines`, `screen_lines`, `columns`.
- `Cell.c` is the char; `Cell.fg` / `Cell.bg` are `vte::ansi::Color`;
`Cell.flags` carries style bits.
- **`Rgb` lives at `alacritty_terminal::vte::ansi::Rgb`**, not `term::color::Rgb`.
- **`SizeInfo` lives at `alacritty_terminal::term::SizeInfo`** in older patches,
but in 0.24.2 it is **not** there. The `TextAreaSizeRequest` callback expects
`alacritty_terminal::event::WindowSize`.

### Terminal modes the app must honour

alacritty's `TermMode` tracks the private modes a program sets. Two of them
change what the *client* must send back:

- **`APP_CURSOR`** (`?1h`, DECCKM) — cursor/Home/End become SS3 (`ESC O A`).
  ncurses programs enable this through terminfo's `smkx` and **only** parse the
  SS3 form.
- **`MOUSE_REPORT_CLICK` / `MOUSE_DRAG` / `MOUSE_MOTION` / `SGR_MOUSE`** —
  the app wants mouse reports instead of local selection.

Verified empirically against ncurses with a Python `curses` probe:

| bytes | ncurses decodes |
|---|---|
| `ESC [ A` | `ESC`, `[`, `A` — three separate keys |
| `ESC O A` | `KEY_UP` |
| `ESC [ < 4 ; 10 ; 5 M` | `BUTTON1_PRESSED` (a phantom click!) |
| `ESC [ < 64 ; 10 ; 5 M` | `BUTTON4_PRESSED` (wheel up) |

The probe lives at `/tmp/opencode/ncurses_probe.py` (recreate it if needed):
a `curses.wrapper` program that logs `getch()` / `getmouse()` results, driven
through a `pty` so the byte sequences can be injected.

### russh 0.45

- `client::Handler` is `#[async_trait]`.
- Server key type is `russh::keys::key::PublicKey`.
- `authenticate_password(...)` returns `Result<bool, _>`.
- `PublicKey::fingerprint()` takes **no arguments** and returns `String`.
- `known_hosts` helpers are at `russh::keys::{check_known_hosts, learn_known_hosts}`.
- `Channel::data` takes `AsyncRead`; a byte slice works.

---

## 11. Features left (ordered by roadmap)

### MVP

1. **Fix `terminal.rs` compile errors** ✅ done (`event::WindowSize` + `vte::ansi::Rgb`).
2. **Copy/paste (F14)** ✅ done. **Keyring password storage (F19)** still open.
3. **Session persistence** — saved profiles in `~/.config/rustterm/sessions.toml`.
4. **Key auth** — `russh::client::Handler::auth_publickey` + ssh-agent.
5. **SFTP panel** — `russh-sftp`, directory listing, follows active session.
6. **Transfers** — upload/download with progress bars.

### V2

7. Edit-and-auto-upload. Watch the **folder**, not the file.
8. X11 forwarding via system `ssh -X`.
9. VNC over SSH tunnel + external viewer.

---

## 12. Next step, concretely

**Step 1 — Fix the build.** ✅ done.
`terminal.rs` now uses `alacritty_terminal::event::WindowSize` for
`TextAreaSizeRequest` and `alacritty_terminal::vte::ansi::Rgb` for `ColorRequest`.
`cargo build` + `cargo clippy -D warnings` are green and the app launches. Still
worth an eyeball check that `htop`/`tmux` do not stall on capability queries.

**Step 2 — Copy/Paste (F14).** ✅ done.
Mouse-drag selection + `Ctrl+Shift+C` copies to the system clipboard;
`Ctrl+Shift+V` pastes via egui's `Event::Paste`. Caveat: egui-winit maps **both**
`Ctrl+C` and `Ctrl+Shift+C` to `Event::Copy` (its `is_copy_command` ignores Shift)
and never delivers the raw key, so the gesture is distinguished by the frame's
current `Modifiers` (`input::is_copy_gesture`), not by the event. `Ctrl+C` with no
Shift still sends `ETX`.

**Step 3 — Session persistence + keyring (F19).** ← next
`~/.config/rustterm/sessions.toml` for profiles, plus `keyring` for passwords.

**Step 3 — Session persistence.**
`~/.config/rustterm/sessions.toml` for profiles, plus `keyring` for passwords.
This unblocks the tab bar from being "type the host every time".

**Step 4 — SFTP.**
`russh-sftp` on a second channel per session, left panel, follows the active
session's `cwd`.

### Working agreement — direct repo access

The assistant has direct read/write access to the working tree at
`~/Desktop/VsTerm`. There is **no** per-step file exchange anymore — do not wait
for the user to paste file contents.

At the start of every implementation step:

1. **Read the real files on disk** before writing anything. Never infer module
   signatures from this handoff.
2. **Build and run the current code first** (`cargo build`, `cargo run`) to
   confirm the baseline before changing it.
3. State which files the step will touch and why.
4. After the change, verify with `cargo build` and `cargo clippy -- -D warnings`,
   then update §16.

Format:

> **Files touched this step:** `src/X.rs`, `src/Y.rs`
> **Verified before/after:** `cargo build` ✅, `cargo clippy` ✅

---

## 13. Open decisions

1. **Tab bar placement** — top bar (classic MobaXterm) ✅ chosen.
2. **Connect form placement** — own tab in the tab bar ✅ chosen.
3. **Session storage format** — TOML at `~/.config/rustterm/sessions.toml`.
4. **Keyring backend** — `keyring` crate with Secret Service on Linux. Needs
verification on Mint 22 before committing.

---

## 14. Environment notes (this machine)

- **Linux Mint 22**, Ubuntu-based. OpenSSH uses **socket activation**
(`ssh.socket`), not `ssh.service`.
- Rust installed via `rustup`; `source "$HOME/.cargo/env"` if `cargo` is not found.
- xkbcommon locale warning at startup is harmless. Permanent fix:

```
sudo locale-gen en_US.UTF-8
sudo update-locale LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8
```
- `vim` is installed. Use it for serious terminal-compat testing.

---

## 15. Git status

Tabs + F1 + F2 + F3 + F4 + F5 + F7 + F8 + F9 all landed on top of the initial
scaffold. `TerminalView` and `app.rs` have been substantially rewritten. `input.rs`
is unchanged.

Before pushing:

- fix `YOUR_USERNAME` in README badges (F20);
- `cargo fmt` and `cargo clippy -- -D warnings`;
- `chmod +x scripts/setup-github.sh`.

`Cargo.lock` **is** committed on purpose — this is a binary crate.

---

## 16. Per-step file tracker

Maintained by the assistant. Update after each successful `cargo build`.

| Step ↕▾ | Files requested ↕▾ | Verified signatures ↕▾ | Status ↕▾ |
|---|---|---|---|
| −Tabs + F1 + F2 | `terminal.rs`, `input.rs` | `TerminalView::new(usize,usize)`, `ui()->Response`, `feed(&[u8])`, `take_resize()->Option<(u32,u32)>`; `event_to_bytes(&Event,Modifiers)->Option<Vec<u8>>` | ✅ done |
| F5 (font metrics) | `terminal.rs`, `main.rs` | `TerminalView::new` signature | ✅ done |
| F8 (scrollback) | `terminal.rs` | `Term::scroll_display` / `renderable_content` paths | ✅ done |
| F4 (batched rows) | `terminal.rs` | `LayoutJob`, `Painter::galley` | ✅ done |
| Repaint gating | `app.rs`, `Cargo.toml` | `poll_countdown`, `request_repaint` | ✅ done |
| F3 (known_hosts) | `ssh.rs`, `app.rs`, `Cargo.toml` | `check_known_hosts`, `learn_known_hosts`, `PublicKey::fingerprint()` | ✅ done |
| F7 (EventProxy) | `ssh.rs`, `terminal.rs`, `app.rs` | `Event::PtyWrite`, `Event::Title`, `Event::Bell`, `Event::TextAreaSizeRequest`, `Event::ColorRequest`, `event::WindowSize`, `vte::ansi::Rgb` | ✅ done |
| F14 (Copy/Paste) | `terminal.rs`, `app.rs`, `input.rs` | `TerminalView::selected_text()`, `clear_selection()`, `select_all()`, `take_action()`, `TermAction`, `input::is_copy_gesture()`, `ViewportCommand::RequestPaste` | ✅ done |
| F14b (Right-click menu) | `terminal.rs`, `app.rs` | `Response::context_menu`, `Ui::close_menu`, `Context::send_viewport_cmd` | ✅ done (keyboard+menu; GUI-unverified) |
| Rendering fix (nano/htop) | `terminal.rs` | `bg_runs()`, `named_color_to_egui`, `indexed_color_to_egui`; selection now drawn over backgrounds | ✅ done (unit-tested; GUI-unverified) |
| F17 (Mouse reporting) | `terminal.rs`, `app.rs` | `TermMode::MOUSE_*`/`SGR_MOUSE`, `TerminalView::forward_mouse()`, `encode_mouse()`, `take_input()` | ✅ done (unit-tested; GUI-unverified) |
| F22 (DECCKM) + wheel codes | `input.rs`, `terminal.rs`, `app.rs` | `TermMode::APP_CURSOR`, `input::cursor_key()`, `wheel_button_code()` | ✅ done (verified against ncurses; GUI-unverified) |
| Next: Session persistence | `app.rs`, `Cargo.toml`, new `sessions.rs` | TBD | ⏳ pending |
⚙

---

## 17. Usage

Feature behaviour, gestures, keybindings, and the per-feature **manual GUI test
checklist** live in [`USAGE.md`](USAGE.md). Keep that file updated as features
land — it is the acceptance-criteria document, not a changelog.
