# Handoff — rustterm (working title)

> **Status:** MVP Terminal & Tabs complete. Core rendering (F1-F5, F7-F9) is
> finished, copy/paste (F14) is done, saved session profiles persist to
> `~/.config/rustterm/sessions.toml`, profiles can optionally store their
> password in the OS keyring, public-key / ssh-agent authentication works, and
> the SFTP side panel is implemented. File transfers are next. Read section 12
> before writing any code.

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
| 1 | SSH terminal with colors, resize, copy/paste | **Done** — colors, resize, scrollback, bg colors, copy/paste. |
| 2 | Multiple SSH sessions in tabs | **Done** |
| 3 | Saved sessions, key auth, keyring passwords | **Done** — profiles, optional keyring passwords, key file + ssh-agent auth. |
| 4 | SFTP side panel following the active session's path | **Mostly done** — per-tab browsing, Home/Up/Refresh and manual paths. Planned follow-ups: reconnect, right-click file operations, hidden-file toggle, copy/paste, owner + modified columns. The panel does not yet follow shell `cd` (no OSC 7). |
| 5 | Upload/download, drag & drop, progress | Not started — small single-file read/write primitives will arrive with the SFTP context menu first. |
| 6 | Open remote file in local editor, auto-upload on save | **Planned** — right-click **Open in local editor** (Zed/gvim/`$VISUAL`/`$EDITOR`/`xdg-open`) will download to a temp file; automatic upload-on-save is V2. |
| 7 | GUI tools via X11 forwarding (`ssh -X`) or VNC-over-tunnel | Not started |

---

## 2. Stack and why

| Concern | Crate | Version | Notes |
|---|---|---|---|
| UI | `eframe` / `egui` | 0.29 | Fastest path to a desktop tool. |
| SSH | `russh` | 0.45 | Pure Rust, no libssh2 C dependency. |
| Terminal emulation | `alacritty_terminal` | 0.24 | **Do not write a custom VT parser.** |
| Async | `tokio` | 1.x | One runtime per SSH session thread. |
| SFTP | `russh-sftp` | 3.0 | Client over a second `russh` channel; server role unused. |
| Error handling | `anyhow` | 1 | |
| Async trait | `async-trait` | 0.1 | Required for `russh::client::Handler`. |
| Logging | `log` + `env_logger` | 0.11 | `RUST_LOG=info cargo run` |
| Profiles | `serde` + `toml` | 1 / 0.8 | `~/.config/rustterm/sessions.toml` |
| Secrets | `keyring` | 4.2 | OS keyring (Secret Service on Linux); opt-in per profile |

`eframe` uses `default-features = false, features = ["glow", "default_fonts"]`.

**Not yet added but will be needed:** `notify` (file watching for auto-upload),
`rfd` (native file dialogs), `dirs` or `directories` (config/temp paths).

---

## 3. Repo layout

```

Cargo.toml                  # eframe/egui 0.29, russh 0.45, russh-sftp 3.0, alacritty_terminal 0.24
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
sftp.rs                   # SftpExplorer: SFTP panel state, navigation, formatting
sessions.rs               # SessionStore: saved profiles in sessions.toml (metadata only)
secrets.rs                # optional OS-keyring password storage (service "rustterm")
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

### Optional keyring feature (packaging)

`keyring` is behind the `keyring-secrets` Cargo feature, which is **on by
default**. It adds the zbus / Secret Service stack: **47 extra crates**
(264 → 311 unique names) and roughly **2.3 MB** of stripped release binary
(13.6 MB → 11.2 MB lean).

```bash
cargo build --release                          # keyring support (default)
cargo build --release --no-default-features    # lean: no keyring, no zbus
```

In a lean build the **Save password in the system keyring** checkbox is hidden,
`src/secrets.rs` compiles a stub that reports "this build has no keyring
support", and passwords are always typed manually. Profiles that were saved
with `save_password = true` still load; they just show that message instead of
auto-filling, and a lean build preserves the flag when re-saving the profile.
The lean build is verified to link no keyring/zbus symbols.

Recommended: publish both, or default to the keyring build and document the
lean variant for minimal installs. Do **not** try to dlopen a Rust crate at
runtime; if truly on-demand secrets are needed later, ship a separate helper
binary and invoke it from the app.

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
                            │                    ▲
                            │     Wake: SSH thread calls ctx.request_repaint()
                            ▼
              app.rs::pump_sessions drains every session (256 KiB/frame
              budget) -> TerminalView::feed()
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
- **Repaint gating:** the SSH thread owns a `Wake` callback
  (`Arc<dyn Fn() + Send + Sync>`) that calls `egui::Context::request_repaint`
  as soon as output arrives. `app.rs` therefore never polls an established
  session; a timer is only used while a connect attempt is pending. Idle CPU is
  ~0%. This replaced the old 8 ms polling window / 3-frame tail, which could
  leave remote echo invisible until the next input event on a high-latency link.
- **All tabs are pumped.** `pump_sessions` drains every session queue, not just
  the visible one, so background SSH windows keep moving and tab switches do
  not dump a backlog. Decoding is capped at 256 KiB per frame and re-requests
  a repaint if work remains.
- **EventProxy holds `input_tx`.** It answers DA1/DA2/DSR queries by writing the
reply bytes back to the PTY. It also handles `Title`, `Bell`, `TextAreaSizeRequest`,
and `ColorRequest`.

### SFTP data flow

```
SftpExplorer::poll  ── sftp_home()/sftp_list() ──► tokio mpsc SftpRequest
                                                       │
                                       run_session select! branch
                                                       │
                                              tokio::spawn(...)
                                                       ▼
                                    SftpSession::canonicalize / read_dir
                                                       │
                                    std::sync::mpsc::Sender<Result<…>>
                                                       │  + Wake
                                                       ▼
                                   SftpExplorer::poll (drained in pump_sessions)
```

- SFTP is a **second channel on the same SSH connection**, opened after the shell
  (and PTY) requests. If the subsystem is missing or its handshake fails, the
  connection still succeeds and the panel shows why SFTP is unavailable.
- The select loop never awaits SFTP: each request is moved into a spawned task, so
  a slow listing cannot delay keystrokes or terminal output. Replies use
  `std::sync::mpsc` and call the egui `Wake` callback.
- `SftpExplorer` is per `ActiveSession`, so each tab keeps its own remote
  directory. It starts at `canonicalize(".")` and is hidden/shown with the
  **Files** toggle in the tab bar.

---

## 7. What works today (verified)

- **Tabs**: Multiple concurrent SSH sessions. `＋ New Session` tab is always present
and not closable. Closing a tab disconnects its SSH cleanly.
- **Saved profiles**: On the New Session tab, fill in host/port/user, optional
  profile name (blank → `user@host`), and press **Save profile**. Profiles are
  written atomically to `~/.config/rustterm/sessions.toml`
  (`$XDG_CONFIG_HOME` respected). Clicking one loads it into the form; **Delete
  selected** removes it. Saving an existing name updates it.
- **Optional keyring passwords (F19)**: tick **Save password in the system
  keyring** before saving. The secret goes to the OS keyring under service
  `rustterm` and account = profile name; an explicit profile name is mandatory
  in that case. Unchecking the box (and saving again) or deleting the profile
  removes the keyring entry. `sessions.toml` still contains metadata only.
  Loading a profile reads the secret from the keyring; if it is missing or the
  keyring is locked/unavailable, the form shows an error and the password can
  be typed manually. Keyring I/O only happens when a profile with the flag is
  loaded/saved/deleted, so users who never opt in are unaffected.
- **Public-key / ssh-agent auth**: tick **Use** on the **Public key** row and
  optionally give a path (blank auto-detects `~/.ssh/id_ed25519`, `id_ecdsa`,
  `id_rsa`, `id_dsa`); tick **Use identities from SSH_AUTH_SOCK** for the agent.
  Order is key file(s) → agent → password, so a typed password remains a
  fallback. `~` is expanded. Passphrase-protected files are not decrypted:
  load them into ssh-agent (`ssh-add`) and enable the agent option. Failures
  list every attempted method (missing file, encrypted key, rejected key,
  agent unavailable, wrong password) instead of a bare "rejected".
- **SFTP side panel**: a second channel per session opens the `sftp` subsystem at
  connect time. The left panel starts in the remote login directory
  (`canonicalize(".")`) and lists the entries with directories first. **Home**,
  **Up**, **Refresh** and an editable path + **Enter**/**Go** navigate; clicking a
  directory opens it and clicking a file selects it (path, size and age in the
  footer). The **Files** toggle in the tab bar, or the panel's **×**, hides it.
  Each tab keeps its own directory. Servers without SFTP show a clear unavailable
  message; terminal I/O is never blocked by a slow listing. Symlinks are shown but
  not followed, and the panel does not yet follow shell `cd` (see F26).
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
- **Legacy SSH servers**: modern KEX/host-key algorithms stay first, with
  `diffie-hellman-group14-sha1` and `ssh-rsa` appended as fallbacks. Verified
  against `172.31.251.23` (OpenSSH 5.3) — the handshake completes and only the
  password decides the outcome. RSA `known_hosts` matching ignores the
  `ssh-rsa` vs `rsa-sha2-256` algorithm-name difference, so the TOFU prompt
  appears once, not on every connect.
- **Interactive latency**: `TCP_NODELAY`, delayed zlib compression
  (`zlib@openssh.com`, like `ssh -C`), and the SSH-thread repaint wake are all
  enabled. Input no longer waits behind Nagle or an expired polling window.
- **Full-speed output**: all sessions are drained every frame with a 256 KiB
  budget; the terminal decoder sustains ~44 MiB/s even in the dev profile, far
  above the link rate. `Cargo.toml` also optimises dev builds (local crates
  `opt-level = 1`, dependencies `opt-level = 2`).

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

**F6 — `vte::Processor::advance` is one byte at a time. (B, mitigated)**
Still a loop, but measured at ~44 MiB/s in the dev profile, which is far above
any SSH link this app targets. `pump_sessions` also caps decoding at 256 KiB
per frame so a burst cannot stall painting. A proper `vte` upgrade would still
reduce CPU for `cat bigfile`, but `alacritty_terminal` 0.24 does not allow one.

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
`terminal.rs` has unit tests for `bg_runs`, the colour mapping and the mouse encoding; `input.rs` has tests for the key encoding incl. DECCKM; `sessions.rs` covers persistence, validation and profile defaults; `sftp.rs` covers path join/parent, sorting and human-readable size/age formatting. **36 tests**, run with `cargo test`. Still missing: automated tests for `ssh.rs` (public-key auth and the SFTP transport were verified with temporary russh-server probes, not committed tests).

**F19 — Password storage / memory hygiene (B, mostly resolved)**
Passwords are never written to `sessions.toml`. Opt-in keyring storage is
implemented (`src/secrets.rs`, keyring 4.2 → Secret Service on Linux) and is
tied to an explicit profile name. The dependency is behind the default-on
`keyring-secrets` feature; `--no-default-features` produces a lean build with a
stub that reports keyring support as unavailable. Remaining work before release:
zeroize the in-memory `String` (`secrecy`/`zeroize`) instead of leaving the
password in a plain `String` until it is replaced or the app exits. Errors are
reported inline; keyring I/O is synchronous on the UI thread but only runs for
user-initiated profile load/save/delete actions.

**F20 / F21 — Repo polish (C)**
Fix `YOUR_USERNAME` in README badges; remove the pointless `.gitignore` rule.

**F23 — Encrypted private-key files (B)**
`secrets`/passphrase input is not implemented; `load_secret_key(path, None)`
returns `KeyIsEncrypted`. The UI explains the workaround (ssh-agent) instead of
prompting. A passphrase modal would be the next refinement.

**F24 — Legacy `ssh-rsa` signatures (B)**
russh-keys 0.45 only signs RSA with `rsa-sha2-256/512`; OpenSSH 5.3 servers
that offer only `ssh-rsa` (SHA-1) will reject key auth. Password auth against
those hosts still works (the current legacy target uses a password). Supporting
them would need a SHA-1 signing path, which is deliberately not a priority.

**F25 — Agent identity errors (C)**
An agent signing error aborts the remaining identities instead of trying the
next one. Rare in practice; revisit if it shows up with hardware tokens.

**F26 — SFTP panel does not follow the shell's working directory (B)**
The panel starts at the login home and navigates independently. There is no
OSC 7 parsing (`Term::current_directory` is not exposed by alacritty_terminal
0.24), so `cd` in the terminal does not move the panel. Automatic following will
need either an OSC 7 hook in the VTE event stream or a periodic remote `pwd`
query, plus a "follow / manual" mode so user navigation is not overridden.
Other limits: no transfers yet, symlinks are displayed but not navigated, and a
server that allows only one session channel will make SFTP unavailable while the
terminal still works.

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
- `known_hosts` helpers are at `russh::keys::{check_known_hosts, learn_known_hosts}`;
  `known_host_keys(host, port)` returns the parsed entries for key-material
  comparison (needed for old `ssh-rsa` host keys, see `known_rsa_key_status`).
- `client::connect` does **not** set `TCP_NODELAY`. Build a `tokio::net::TcpStream`,
  `set_nodelay(true)`, then call `client::connect_stream(config, socket, handler)`.
- Compression names: `russh::compression::{ZLIB, ZLIB_LEGACY}`. The default
  preference list is `[NONE, ZLIB, ZLIB_LEGACY]`, so compression is **off**
  until reordered. Both zlib variants are initialised only after authentication.
- `Channel::data` takes `AsyncRead`; a byte slice works.
- Public-key auth: `russh::keys::load_secret_key(path, None)` →
  `KeyPair` (`Error::KeyIsEncrypted` for passphrase files),
  `Handle::authenticate_publickey(user, Arc<KeyPair>)`.
- Agent auth: `AgentClient::connect_env()` → `request_identities()` →
  `Handle::authenticate_future(user, key, agent)`. That call **consumes** the
  agent and returns `(agent, Result<bool, AgentAuthError>)`, so the same
  connection is reused for the next identity.
- `KeyPair::RSA` carries a `SignatureHash` limited to SHA2-256/512; there is no
  SHA-1 path, so `ssh-rsa`-only servers reject key auth (see F24).

### russh-sftp 3.0

- Client: open a second channel, `channel.request_subsystem(true, "sftp")`, then
  `SftpSession::new_with_config(channel.into_stream(), Config { request_timeout_secs,
  .. })`. The stream needs `AsyncRead + AsyncWrite + Unpin + Send + 'static`.
  Create/handshake it **inside the session's tokio runtime**.
- `SftpSession::canonicalize(".")` resolves the remote login directory.
- `SftpSession::read_dir(path).await` returns a `ReadDir` iterator of
  `DirEntry` (`file_name() -> String`, `path() -> String`, `metadata() ->
  Metadata`). `Metadata::file_type()` is a `FileType` with `is_dir()` /
  `is_symlink()`; `len()` is `u64`; `modified()` is `io::Result<SystemTime>`.
  `read_dir` internally loops `READDIR` until `StatusCode::Eof`, so a server
  handler must return `Err(StatusCode::Eof)` after the last batch.
- `russh_sftp::server::Handler` uses RPITIT (`fn … -> impl Future + Send`), **not**
  `#[async_trait]`, unless the crate's `async-trait` feature is enabled. The
  bundled russh 0.45 `examples/sftp_server.rs` predates 3.0 and still uses
  `#[async_trait]`; do not copy it verbatim.
- `SftpSession` is not `Clone`; wrap it in `Arc` for spawned request tasks.
- No SFTP request may be awaited in `run_session`'s `select!` directly: spawn it,
  or a slow listing stalls terminal I/O.

---

## 11. Features left (ordered by roadmap)

### MVP

1. **Fix `terminal.rs` compile errors** ✅ done (`event::WindowSize` + `vte::ansi::Rgb`).
2. **Copy/paste (F14)** ✅ done. **Keyring password storage (F19)** ✅ done.
3. **Session persistence** ✅ done — saved profiles in
   `~/.config/rustterm/sessions.toml` (`src/sessions.rs`). Passwords stay out of
   the file; keyring storage is opt-in (step 4).
4. **Keyring password storage (F19)** ✅ done — opt-in per profile via
   `src/secrets.rs` + `keyring` 4.2. Missing/locked keyrings fall back to
   manual password entry. In-memory zeroization is still tracked under F19.
5. **Key auth** ✅ done — key files + ssh-agent before the password.
6. **SFTP panel** ✅ done — `russh-sftp` on a second channel, per-tab directory
   listing, Home/Up/Refresh + editable path. Shell-cwd following is tracked as
   F26.
7. **SFTP panel follow-ups** ← next
   - **Reconnect** — a button (and an offer when a request fails) that reopens
     only the SFTP channel/subsystem on the existing SSH connection, then restores
     the current path. It must never tear down the terminal. If the SSH
     connection itself is gone, use the session-level reconnect flow instead.
   - **Right-click row menu** (also Menu key / Shift+F10): Open, Download…,
     Upload here…, Rename…, Delete…, New folder, Copy, Paste, Copy path, Refresh.
     Destructive actions confirm first; deleting a non-empty directory needs an
     explicit recursive confirmation.
   - **Open in local editor** — download the selected file to a temp path and
     launch `$VISUAL` / `$EDITOR`, with presets for **Zed** and **gvim** and the
     desktop default via `xdg-open`. Launch asynchronously; never block the UI.
     Full save-watching/auto-upload stays in V2.
   - **Show/hide hidden files** — a panel toggle (persisted per session, default
     hidden) that filters dotfiles client-side and survives Refresh/navigation.
   - **Copy / paste** — in-panel clipboard for remote paths and files. Bind
     `Ctrl+C`/`Ctrl+V` only while the panel has focus so terminal keys are not
     stolen; always offer the actions in the context menu. SFTP has no
     server-side copy operation, so duplicating a file/directory requires a
     read+write stream helper (or `cp -r` over an exec channel if allowed);
     **Copy path** needs no I/O.
   - **Owner + modified columns** — each row shows the owner and last-modified
     date/time (plus size). `Metadata` exposes `uid`, `user`, `gid`, `group` and
     `mtime`; OpenSSH usually omits `user`, so show the server-provided name when
     present, otherwise the numeric UID or a cached `/etc/passwd` lookup fetched
     once per session. Use a monospace, right-aligned timestamp column and keep
     the name column flexible.
8. **Transfers** — upload/download with progress bars; reuse the single-file
   read/write primitives introduced by the context menu.

### V2

9. Edit-and-auto-upload. Watch the **folder**, not the file.
10. X11 forwarding via system `ssh -X`.
11. VNC over SSH tunnel + external viewer.

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

**Step 3 — Session persistence.** ✅ done.
`src/sessions.rs` loads/saves `~/.config/rustterm/sessions.toml`
(`$XDG_CONFIG_HOME` respected) with atomic writes. The New Session tab can save,
load and delete profiles. Only name/host/port/user are stored; a blank name
defaults to `user@host`, and saving an existing name updates that profile.
Load/save failures are reported inline and never prevent startup.

**Step 3b — Keyring password storage (F19).** ✅ done.
`src/secrets.rs` wraps `keyring` 4.2 (Secret Service on Linux) with service
`rustterm` and account = profile name. The New Session form has an opt-in
**Save password in the system keyring** checkbox; enabling it makes an explicit
profile name mandatory. Storing happens before the profile is written, so the
TOML never claims a password exists when the keyring failed. Unchecking or
deleting the profile removes the entry. Read failures degrade to manual entry.
Verified end-to-end against the running GNOME Keyring.

**Step 4 — Key auth.** ✅ done.
`src/ssh.rs` now authenticates in the order key file(s) → ssh-agent → password.
The form has **Public key** (Use + optional path; blank auto-detects
`~/.ssh/id_*`) and **SSH agent** (`SSH_AUTH_SOCK`) rows. `~` is expanded.
Failures are collected per method and reported together. Profiles gained
`use_key`, `key_path` and `use_agent` (all default-off, so old files load
unchanged). Verified end-to-end against a temporary russh server: file key,
agent identity and password all succeed; an encrypted key produces the
passphrase/agent guidance. GUI checklist in `USAGE.md` §13.

**Step 5 — SFTP.** ✅ done.
`src/ssh.rs` opens a second channel after the shell, requests the `sftp`
subsystem and builds an `SftpSession` with a 10 s request timeout. Failures are
non-fatal: `SshSession::sftp_unavailable()` carries the reason. The UI-facing API
is `sftp_home()` / `sftp_list(path)`; both return `std::sync::mpsc::Receiver`s
and wake egui on completion. `run_session` spawns each request so the terminal
select loop never awaits the network.

`src/sftp.rs` holds `SftpExplorer` (current path, editable path, entries,
selection, loading/error/unavailable state, one in-flight `Job`). It is stored per
`ActiveSession`. `src/app.rs` polls every tab in `pump_sessions`, draws a
resizable left `SidePanel` when `show_sftp` is on, and adds the **Files** toggle
to the tab bar. Navigation: **Home** (`canonicalize(".")`), **Up**, **Refresh**;
the path box navigates on Enter; **Go** does the same; clicking a directory opens
it and clicking a file selects it.

Verified with 36 tests, clippy/fmt clean, and a temporary in-process russh SFTP
server probe: `canonicalize(".")` → `/probe`, `read_dir` returned a directory and
a regular file with the expected `is_dir`, `len` and `modified` values.

Limits (F26): the panel does not follow shell `cd` (no OSC 7), symlinks are not
navigated, and transfers are not implemented. Manual GUI checklist in
`USAGE.md` §14.

**Step 6 — SFTP panel follow-ups.** ← next
Reconnect (SFTP channel only), right-click row menu (delete, rename, new folder,
download/upload, copy/paste, copy path, open in local editor), hidden-file
toggle, and owner + modified columns. Details and acceptance notes are in §11;
this step also introduces the single-file read/write primitives reused by
transfers. Keep every operation off the terminal select loop and confirm
destructive actions.

**Step 7 — Transfers.**
Upload/download over the existing `SftpSession`, with progress reporting, a
transfer queue, and drag & drop. Runs off the terminal select loop like the
existing SFTP requests.

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
4. **Keyring backend** — `keyring` crate with Secret Service on Linux ✅
   verified against GNOME Keyring. Behind the default-on `keyring-secrets`
   feature so packagers can build lean with `--no-default-features`.
5. **On-demand keyring download** — rejected for now: Rust cannot load a crate
   at runtime. A separate helper binary would be the only option if it is ever
   required.

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
- Saved profiles: `~/.config/rustterm/sessions.toml` (or
  `$XDG_CONFIG_HOME/rustterm/sessions.toml`). Safe to delete to start fresh; it
  contains no secrets.
- Keyring entries: service `rustterm`, account = profile name. Inspect/remove
  them with **Seahorse** (Passwords and Keys) or
  `secret-tool search service rustterm`. Deleting a profile in the app removes
  its entry; editing the TOML by hand does not.
- Keyring used for verification: GNOME Keyring (`gnome-keyring-d`) over the
  user's D-Bus session. On systems without a Secret Service, password saving
  fails with an inline error and manual entry still works.

---

## 15. Git status

Tabs + F1 + F2 + F3 + F4 + F5 + F7 + F8 + F9, saved profiles, keyring
passwords, public-key/agent auth and the SFTP panel have landed on the `feat-tabs`
branch. The SFTP step (`src/sftp.rs` + `ssh.rs`/`app.rs` wiring, `russh-sftp`
dependency) is implemented and verified but may still be uncommitted — check
`git status`.

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
| Repaint gating | `app.rs`, `Cargo.toml` | `poll_countdown`, `request_repaint` | ✅ superseded |
| Legacy SSH + performance | `ssh.rs`, `app.rs`, `Cargo.toml` | `SshSession::connect(config, prompt_tx, wake)`, `Wake`, `client_config()`, `known_rsa_key_status()`, `connect_stream`+`set_nodelay`, `compression::ZLIB_LEGACY`, `pump_sessions(ctx)`, `MAX_OUTPUT_PER_FRAME` | ✅ done (legacy handshake verified; user confirmed the speed-up) |
| F3 (known_hosts) | `ssh.rs`, `app.rs`, `Cargo.toml` | `check_known_hosts`, `learn_known_hosts`, `PublicKey::fingerprint()` | ✅ done |
| F7 (EventProxy) | `ssh.rs`, `terminal.rs`, `app.rs` | `Event::PtyWrite`, `Event::Title`, `Event::Bell`, `Event::TextAreaSizeRequest`, `Event::ColorRequest`, `event::WindowSize`, `vte::ansi::Rgb` | ✅ done |
| F14 (Copy/Paste) | `terminal.rs`, `app.rs`, `input.rs` | `TerminalView::selected_text()`, `clear_selection()`, `select_all()`, `take_action()`, `TermAction`, `input::is_copy_gesture()`, `ViewportCommand::RequestPaste` | ✅ done |
| F14b (Right-click menu) | `terminal.rs`, `app.rs` | `Response::context_menu`, `Ui::close_menu`, `Context::send_viewport_cmd` | ✅ done (keyboard+menu; GUI-unverified) |
| Rendering fix (nano/htop) | `terminal.rs` | `bg_runs()`, `named_color_to_egui`, `indexed_color_to_egui`; selection now drawn over backgrounds | ✅ done (unit-tested; GUI-unverified) |
| F17 (Mouse reporting) | `terminal.rs`, `app.rs` | `TermMode::MOUSE_*`/`SGR_MOUSE`, `TerminalView::forward_mouse()`, `encode_mouse()`, `take_input()` | ✅ done (unit-tested; GUI-unverified) |
| F22 (DECCKM) + wheel codes | `input.rs`, `terminal.rs`, `app.rs` | `TermMode::APP_CURSOR`, `input::cursor_key()`, `wheel_button_code()` | ✅ done (verified against ncurses; GUI-unverified) |
| Session persistence | `sessions.rs` (new), `app.rs`, `main.rs`, `Cargo.toml` | `SessionStore::{load_default,load,upsert,remove,profiles,path,error}`, `SessionProfile::validate`, `save_profile_ui`, `saved_sessions_ui`, `load_profile` | ✅ done (user-verified in GUI) |
| Keyring passwords (F19) | `secrets.rs` (new), `sessions.rs`, `app.rs`, `main.rs`, `Cargo.toml` | `secrets::{save,load,delete}`, `SessionProfile::save_password`, `save_profile_ui`, keyring 4.2 behind `keyring-secrets` | ✅ done (default + lean builds; 29 tests each; keyring flow verified on GNOME Keyring) |
| Public-key / agent auth | `ssh.rs`, `app.rs`, `sessions.rs` | `SshConfig::{use_key,key_path,use_agent}`, `authenticate()`, `authenticate_with_agent()`, `key_candidates()`, `expand_tilde()`, `SessionProfile::{use_key,key_path,use_agent}` | ✅ done (31 tests; file key + agent + password verified against a temporary russh server; GUI-unverified) |
| SFTP panel | `sftp.rs` (new), `ssh.rs`, `app.rs`, `main.rs`, `Cargo.toml` | `SshSession::{sftp_unavailable,sftp_home,sftp_list}`, `RemoteEntry`, `SftpExplorer::{start,poll,load_home,navigate,refresh,up,selected_entry}`, `join_path`, `parent_path`, `sort_entries`, `format_size`, `format_age`, `RustTermApp::sftp_ui`, `show_sftp` | ✅ done (36 tests; transport verified against a temporary in-process russh SFTP server; GUI-unverified) |
| Next: SFTP follow-ups | `sftp.rs`, `ssh.rs`, `app.rs`, `Cargo.toml` | TBD — reconnect, right-click menu, open-in-editor, hidden toggle, copy/paste, owner + mtime columns | ⏳ pending |
| Then: transfers | `ssh.rs`, `sftp.rs`, `app.rs` | TBD — upload/download, progress, queue, drag & drop | ⏳ pending |
⚙

---

## 17. Usage

Feature behaviour, gestures, keybindings, and the per-feature **manual GUI test
checklist** live in [`USAGE.md`](USAGE.md). Keep that file updated as features
land — it is the acceptance-criteria document, not a changelog.
