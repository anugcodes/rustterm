# Handoff — rustterm (working title)

> **Status:** MVP Terminal & Tabs complete. Core rendering (F1-F5, F7-F9) is
> finished, copy/paste (F14) is done, saved session profiles persist to
> `~/.config/rustterm/sessions.toml`, profiles can optionally store their
> password in the OS keyring, public-key / ssh-agent authentication works, and
> the SFTP side panel is implemented with reconnect, file operations, hidden-file
> filtering, copy/paste, open-in-local-editor and owner/modified rows. File
> transfers are next. Read section 12 before writing any code.

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
| 3 | Saved profiles, key auth, keyring passwords | **Done** — profiles, optional keyring passwords, key file + ssh-agent auth. |
| 4 | SFTP side panel following the active session's path | **Done except shell-cd following** — per-tab browsing, resizable Name/Owner/Modified/Size table, reconnect, right-click menu, hidden toggle, copy/paste, status bar with history. The panel does not follow shell `cd` (no OSC 7), and symlinks are not navigated. |
| 5 | Upload/download, drag & drop, progress | **Next** — copy/editor downloads already stream file contents; general transfers, progress and drag & drop are not implemented. |
| 6 | Open remote file in local editor, auto-upload on save | **Partial** — the **Open with…** picker downloads to a temp file and launches `$VISUAL`/`$EDITOR`, Zed, gvim, VS Code, the desktop default or a custom command. Automatic upload-on-save is V2. |
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
| Timestamps | `chrono` | 0.4 | Local-time formatting for SFTP rows |

`eframe` uses `default-features = false, features = ["glow", "default_fonts"]`.

**Not yet added but will be needed:** `notify` (file watching for auto-upload),
`rfd` (native file dialogs), `dirs` or `directories` (config/temp paths).

---

## 3. Repo layout

```

Cargo.toml                  # eframe/egui 0.29, russh 0.45, russh-sftp 3.0, alacritty_terminal 0.24, chrono 0.4, toml
Cargo.lock                  # COMMITTED (binary crate — reproducible builds)
README.md                   # project overview, features budget and build instructions
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
sftp.rs                   # SftpExplorer: panel state, operations, dialogs, editor launch, formatting
sessions.rs               # SessionStore: saved profiles in sessions.toml (metadata only)
settings.rs               # persisted UI preferences (theme) in settings.toml
theme.rs                  # theme registry, palette/metrics tokens, widget helpers
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
SftpExplorer::poll ── sftp_home/list/rename/delete/create_dir/copy/fetch() ──►
                              tokio mpsc SftpRequest
                                       │
                       run_session select! branch
                          │                       │
             Reconnect ───┘                       └── tokio::spawn(...)
                 │                                          ▼
        open_sftp(&Handle)                    Arc<SftpClient> clone + action
        swap SharedSftp                                 │
                                          canonicalize / read_dir / open+copy /
                                          create_dir / rename / remove / symlink
                                                        │
                                   std::sync::mpsc::Sender<Result<…>>
                                                        │  + Wake
                                                        ▼
                                  SftpExplorer::poll (drained in pump_sessions)
```

- SFTP is a **second channel on the same SSH connection**, opened after the shell
  (and PTY) requests. If the subsystem is missing or its handshake fails, the
  connection still succeeds and the panel shows why SFTP is unavailable.
- The live client is `Arc<Mutex<Option<Arc<SftpClient>>>>`. Reconnect opens a new
  channel/subsystem through the shared `Arc<Handle<ClientHandler>>`, swaps the
  client and closes the old channel; the terminal session is untouched.
- The select loop never awaits SFTP: listings and file operations clone the
  current client and run in a spawned task, so a slow listing or a large copy
  cannot delay keystrokes or terminal output. Replies use `std::sync::mpsc` and
  call the egui `Wake` callback.
- `SftpClient` caches `/etc/passwd` in a `tokio::sync::OnceCell`, loaded on the
  first listing. Rows use the cached name, then `Metadata.user`, then the UID.
- Remote copy/paste stores a path and is implemented as a recursive streamed
  copy (`tokio::io::copy`) with unique `name (copy N)` destinations. Symlinks are
  recreated, not followed.
- `SftpExplorer` is per `ActiveSession`, so each tab keeps its own remote
  directory, clipboard, hidden-file toggle and open dialog. It starts at
  `canonicalize(".")` and is hidden/shown with the **Files** toggle in the tab
  bar.

---

## 7. What works today (verified)

- **Tabs**: Multiple concurrent SSH sessions. `+ New Session` tab is always present
and not closable. Each tab is a single box (minimum 168 px) with the label and
the close `×` inside it; middle-click also closes. The theme picker is pinned to
the far right of the tab bar.
- **New Session page**: a centred column (maximum 600 px) inside a scroll area.
  One card holds the connection fields, the auth options, an optional **Save as
  profile** checkbox with its name field, the primary Connect button and an
  inline status line. Below a separator is the compact **Saved profiles** table
  (**Name / Target**, **Load**, per-row **×**) with an empty-state hint. Labels
  share one gutter and all inputs are 26 px with vertically centred text.
- **Themes**: a shared `theme::Palette`/`Metrics` registry styles the whole app
  (top bar, cards, tables, dialogs, status bar). **Midnight** (default) and
  **Graphite** ship today; the choice is persisted in
  `~/.config/rustterm/settings.toml` (`$XDG_CONFIG_HOME` respected). Terminal
  ANSI colours are independent of the UI theme. The design is flat and boxy:
  zero rounding, no shadows, hover changes fill only, 1 px borders, floating
  scrollbars and one shared padding scale.
- **Saved profiles**: On the New Session tab, fill in host/port/user, tick
  **Save as profile**, optionally type a profile name (blank → `user@host`), and
  press **Connect**. The profile is written *before* the connection starts, so a
  validation error aborts the attempt instead of connecting silently. Profiles
  are written atomically to `~/.config/rustterm/sessions.toml`
  (`$XDG_CONFIG_HOME` respected). **Load** fills the form back in; the row's
  **×** deletes it. Saving an existing name updates it in place.
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
  connect time. The left panel (450 px default, 340 px minimum, capped at 55% of
  the window or 720 px) starts in the remote login directory
  (`canonicalize(".")`) and renders a fixed-header table with **Name / Owner /
  Modified / Size** columns (hand-rolled, elided, striped). Column dividers exist
  only in the header; dragging one resizes the column to its right. The header
  uses the same font size and weight as the rows.
  Single-click selects; double-click opens a folder or brings up **Open with…**
  for a file; right-click selects and opens the row menu. **Home**, **Up**,
  **Refresh**, **Reconnect** and an editable path + **Enter**/➡ navigate.
- **SFTP operations**: right-click a row for **Open**/**Open with…**,
  **Rename…**, **Copy path**, **Delete…**, plus **Paste into this folder** on
  folders when the clipboard has an entry. Delete confirms first and removes
  non-empty directories recursively. **Copy path** uses the system clipboard;
  remote **Copy**/**Paste** streams a recursive duplicate with a unique
  `(copy)` name. The **Hidden** toggle filters dotfiles client-side (default
  hidden, per session). Operations run off the UI thread and refresh the listing
  on completion.
- **SFTP status bar**: the bottom strip shows the latest message (info/success/
  error), a spinner plus operation label while busy, and a ☰ history popover with
  the last 50 messages and **Clear messages**. The old selected-file footer and
  inline error block are gone; row tooltips carry file details.
- **SFTP reconnect** reopens only the SFTP channel/subsystem on the existing SSH
  connection and then resolves the home directory again. It is available both in
  the toolbar and in the unavailable state; the terminal is never disconnected.
  If the server had no SFTP subsystem at login, this is the way to retry.
- **Open with…** opens a modal picker (Default editor, Zed, gvim, VS Code,
  Desktop default, Custom command). Editors missing from `PATH` are disabled,
  and the last choice is remembered per session. The selected file is
  downloaded to a unique `rustterm-edit-<pid>-<nanos>` temp directory and the
  editor is launched asynchronously. Save-watching/auto-upload is still V2.
- The **Files** toggle in the tab bar, or the panel's **×**, hides the panel. Each
  tab keeps its own directory. Servers without SFTP show a clear unavailable
  message; terminal I/O is never blocked by a slow listing. Symlinks are shown but
  not followed, and the panel does not follow shell `cd` (see F26).
- **File transfers**: uploads and downloads stream in 64 KiB chunks over the
  existing `SftpSession`, write to a sibling `.part` file and rename only on
  success. One transfer runs per session and the rest queue; byte/file progress
  and the cancel switch are atomics read once per frame, so the terminal stays
  responsive. Existing destinations prompt for **Replace / Keep both / Cancel**
  before anything is written, folder trees and symlinks are recreated in both
  directions, drag & drop uploads into the visible directory, and the listing
  refreshes when the transfer ends. Details and the GUI checklist are in
  `USAGE.md` §17.
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
**F27** — Uploads silently killed the SSH connection ✅ Root cause was **not** the SFTP crate: `russh 0.45.0`'s outbound zlib compressor reserves `input.len() + 10` bytes and breaks its loop as soon as `flate2` returns `Ok`, even when the output buffer filled and input remains. The tail of an incompressible packet (PDF, PNG, zip) was dropped, the server saw a malformed packet and closed the transport; the SFTP request map then produced `sender dropped`. Text uploads only worked because their compressed output fit. Upstream fixed this later (russh ≥ 0.65, test `incompressible_packets_round_trip`). The client now advertises `compression::NONE` (unit test `client_config_advertises_no_compression`). **Tech debt:** once russh is upgraded, compression can be re-enabled; `russh-sftp 3.0.1` is transport-agnostic, so the upgrade is independent of the SFTP crate. Dead sessions are now detected via `SshSession::is_alive()` and the tab shows a **Connection lost** banner + **Close tab**.

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
`terminal.rs` has unit tests for `bg_runs`, the colour mapping and the mouse encoding; `input.rs` has tests for the key encoding incl. DECCKM; `sessions.rs` covers persistence, validation and profile defaults; `sftp.rs` covers path join/parent, sorting, formatting, name validation, copy guard and editor resolution; `ssh.rs` covers `/etc/passwd` parsing, remote path joining, the no-compression config and connection-loss classification. **55 tests**, run with `cargo test`. Still missing: automated network tests for public-key auth and the SFTP transport (both were verified with temporary russh-server probes, not committed tests).

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

**F20 / F21 — Repo polish (C)** ✅ done
README badges point at `anugcodes`, and the stale `.gitignore` rule is gone.

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
Other limits: no general upload/download/drag & drop yet (copy and editor
downloads use streaming helpers that transfers can reuse), symlinks are displayed
but not navigated, and a server that allows only one session channel will make
SFTP unavailable while the terminal still works. **Reconnect** can retry the SFTP
channel in that case.

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
7. **Only bundled fonts are available.** Proportional text falls back through
   Ubuntu-Light → NotoEmoji → emoji-icon-font. Glyphs outside those fonts render
   as tofu boxes: avoid `＋`, `⌂`, `↑`, `→`, `⧉`, `◉`, `◐`, `✓`, `●`, `⋯`.
   Use `+`, `🏠`, `⬆`, `➡`, `🗀`, `🗋`, `🗐`, `👁`, `✔`, `•`, `☰`, `🎨` or
   plain text.
8. **Hand-rolled tables need `inner_rect`, not `available_width`.** The SFTP
   listing draws the header *after* the scrolling body, using
   `ScrollArea::show_rows` and `output.inner_rect.width()`; `available_width`
   there included the scrollbar and misaligned the columns. Capture row actions
   in an `Option` and apply them after the loop, because the explorer cannot be
   borrowed mutably while it is being iterated.
9. **`egui::Window` has a 420 px default inner height.** A short modal therefore
   looks empty. `.auto_sized()` is not the fix — it sets an infinite default size
   and expands to the whole area. Use `.default_size(vec2(width, 1.0))` with a
   fixed inner width so the window tracks its content.

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
  `russh::client::Handle` is also **not** `Clone`; the session thread keeps it in
  an `Arc` so reconnect tasks can open channels.
- No SFTP request may be awaited in `run_session`'s `select!` directly: spawn it,
  or a slow listing stalls terminal I/O.
- `Metadata` is `FileAttributes`: `size`, `uid`, `user`, `gid`, `group`,
  `permissions`, `atime`, `mtime` are all `Option`. `modified()` builds a
  `SystemTime` from `mtime`.
- `SftpSession::open`/`create` return a `File` that implements
  `AsyncRead`/`AsyncWrite`; `close(self)` is async and consumes it. Use
  `tokio::io::copy` then flush/close for streamed remote copies and downloads.
- `SftpSession::symlink(target, link_path)` follows OpenSSH's reversed wire order
  (the crate's parameter names are `path, target` but the first value is the
  target). Verify against a real server before changing this.
- `SftpSession::symlink_metadata` is the safe way to distinguish a symlink from
  its target when walking a tree for recursive copy/delete.

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
7. **SFTP panel follow-ups** ✅ done.
   - **Reconnect** — toolbar and unavailable-state buttons reopen only the
     SFTP channel/subsystem on the existing SSH connection. The terminal is never
     torn down; the home directory is resolved again after the swap.
   - **Right-click row menu** — file: Open with…, Rename…, Copy path, Delete…;
     folder: Open, Paste into this folder (when copied), Rename…, Copy path,
     Delete…. Delete confirms first and removes non-empty directories
     recursively. **Download…/Upload here…** remain in transfers.
   - **Open with…** — modal picker (default, Zed, gvim, VS Code, desktop,
     custom command), unique temp download and asynchronous launch. No save
     watching.
   - **Show/hide hidden files** — per-session toggle, default hidden; filters
     dotfiles client-side and survives Refresh/navigation. The selection is
     cleared if it becomes hidden.
   - **Copy / paste** — toolbar + context menu. SFTP has no server-side copy, so
     files stream through `tokio::io::copy`; directories recurse; symlinks are
     recreated with their original target; conflicts get `name (copy N)`.
   - **Owner + modified columns** — the table shows Name, Owner, Modified and
     Size. Owner precedence is cached `/etc/passwd`, then `Metadata.user`, then
     UID. Details live in row tooltips and the status bar.
   - Keyboard `Ctrl+C`/`Ctrl+V` were deliberately **not** bound in the panel;
     terminal copy/paste must not be stolen. The toolbar/context menu are always
     available.
8. **UI overhaul** ✅ done — hand-rolled fixed-header SFTP table with
   header-only column resizing, icon toolbar with tooltips, bottom status bar
   with history, font-safe glyphs, **Open with…** modal, compact content-sized
   dialogs, flat boxy theme with a global persisted registry (`src/theme.rs`,
   `src/settings.rs`), in-tab close buttons, and a compact New Session page with
   saved-profile management folded into the connection card.
9. **Transfers** ✅ done — recursive upload/download with byte and file progress,
   a per-session queue, a cancel switch, a conflict prompt (Replace / Keep both /
   Cancel), `.part` + rename so interrupted transfers never overwrite a good
   file, and drag & drop. Entry points: ⬇/⬆ toolbar, row menu and the panel
   itself. `rfd` supplies the local folder/file pickers. GUI checklist in
   `USAGE.md` §17.

### V2

10. Edit-and-auto-upload. Watch the **folder**, not the file.
11. X11 forwarding via system `ssh -X`.
12. VNC over SSH tunnel + external viewer.

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

**Step 6 — SFTP panel follow-ups.** ✅ done.
`ssh.rs` now exposes `sftp_reconnect`, `sftp_rename`, `sftp_delete`,
`sftp_create_dir`, `sftp_copy` and `sftp_fetch`, all returning
`std::sync::mpsc::Receiver`s and waking egui. `SftpClient` owns the channel plus
a lazy `/etc/passwd` owner cache; `SharedSftp` (`Arc<Mutex<Option<Arc<_>>>>`)
allows reconnect to swap the channel while the terminal loop keeps running.
`run_session` handles reconnect specially (it needs the shared `Arc<Handle>`) and
spawns every other action. Copy/deletes use lstat-based recursion, stream files,
recreate symlinks and clean up partial destinations.

`SftpExplorer` gained hidden-file filtering, a remote clipboard, dialogs
(rename/new folder/delete), an operation receiver with progress labels,
editor launch, exact timestamp formatting, and testable name/copy guards.
`app.rs` draws two-line rows (name + owner/timestamp), the extra toolbar, row
context menus and the modal dialogs. `chrono` was added for local timestamps.

Verified with **42 tests** at the time (48 today; both feature sets), clippy
`-D warnings`, fmt and a release build. GUI file-operation probing against a live server is still in the
manual checklist (`USAGE.md` §14), including recursive delete, copy of files and
folders, symlink copy, hidden filtering, editor launch and reconnect.

**Step 7 — UI overhaul.** ✅ done.
The SFTP listing (Name/Owner/Modified/Size) is hand-rolled: a fixed header plus
`ScrollArea::show_rows`, with column dividers only in the header. `egui_extras`
was removed. The selected-file footer and inline error block were replaced by a
bottom status bar backed by `StatusLog` (last 50 messages, ☰ history, Clear).
The toolbar is icon-only with tooltips; right-click is the only menu gesture.
**Open with…** is a modal with availability checks and a custom command. Dialogs
share `theme::modal_frame`, size to their content and right-align their actions.
The New Session page is a compact centred column: one connection card (with
auth options and the optional **Save as profile** field) plus a **Name/Target**
profile table. `src/theme.rs` + `src/settings.rs` provide a flat, persisted theme
registry (Midnight, Graphite) in `~/.config/rustterm/settings.toml`; corners are
square, shadows and hover glow are gone, and one padding/control-height scale is
shared by forms, tables, dialogs and tabs.

Verified with **48 tests** (both feature sets), clippy `-D warnings`, fmt and a
release build. On screen: New Session at 900×700 and 700×400, the tab close `×`,
panel cap/hide/show, header resizing, the SFTP rename/new-folder/delete and
**Open with…** dialogs, the host-key prompt (accept and reject) and theme
switching/persistence. The remaining live-SFTP items are in `USAGE.md` §14.

**Step 8 — Transfers.** ✅ done.
`SftpAction::{Upload,Download}` stream in 64 KiB chunks through the session thread
(`upload_items` / `download_items`, with recursive `collect_upload` /
`collect_download` pre-walks for the byte total). `TransferProgress` reports
bytes, files and cancellation through atomics. `SftpExplorer` keeps one active
transfer plus a `VecDeque` queue, asks about conflicts before starting and polls
the result each frame. UI: ⬇/⬆ toolbar buttons, row-menu Download / Upload here,
a progress bar above the status bar, a conflict modal and desktop drag & drop.
Local pickers use `rfd` (`xdg-portal` + `tokio`). Verified with 53 tests in both
feature sets; live GUI checks are listed in `USAGE.md` §17.

**Step 9 — Edit-and-auto-upload (V2).** ← next
Watch the **folder** a temp download was opened from, not just the file, and push
changes back over SFTP with the same transfer backend.

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
passwords, public-key/agent auth, the SFTP panel and its follow-up operations,
plus the UI overhaul (table, icon toolbar, status bar, **Open with…** modal,
dialogs, New Session page and themes) have landed on `main`. `cargo test` (48),
clippy (both feature sets) and the release build are green. GUI verification of
the New Session page, tab bar and theme switching is done; live-SFTP table/
dialog testing still needs a server with credentials. Check `git status` before
starting the transfers step.

Before pushing:

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
| SFTP follow-ups | `sftp.rs`, `ssh.rs`, `app.rs`, `Cargo.toml` | `SshSession::{sftp_reconnect,sftp_rename,sftp_delete,sftp_create_dir,sftp_copy,sftp_fetch}`, `SftpClient`, `SharedSftp`, `RemoteEntry::owner`, `SftpExplorer::{reconnect,copy_entry,paste,paste_into,begin_rename,begin_delete,begin_new_folder,begin_new_folder_in,submit_dialog,open_in_editor}`, `SftpDialog`, `EditorChoice`, `format_timestamp`, `RustTermApp::{sftp_ui,sftp_dialogs_ui}`, `chrono` | ✅ done (42 tests both feature sets; clippy/fmt/release clean; GUI-unverified) |
| UI overhaul | `theme.rs` (new), `settings.rs` (new), `app.rs`, `sftp.rs`, `main.rs`, `Cargo.toml` | `theme::{ThemeId,Theme,Palette,Metrics,apply,body_font,panel_frame,card_frame,modal_frame,callout_frame,section_label,muted,primary_button,danger_button,icon_button,icon_toggle}`, `Settings::{load_default,load_from,save_default,save_to}`, `SftpExplorer::status`, `StatusLog`, `EditorPicker`, `SftpExplorer::{begin_open_with,submit_open_with,cancel_open_with}`, `RustTermApp::{tabs_ui,tab_button,theme_menu_ui,editor_picker_ui,text_field,form_label,sized_label,sftp_header,SftpColumns,SftpLayout}`, hand-rolled SFTP table (`ScrollArea::show_rows`), `egui_extras` removed | ✅ done (48 tests; clippy/fmt/release clean; New Session, tabs, panel, dialogs, host-key prompt and themes verified in the GUI; remaining live-SFTP items in `USAGE.md` §14) |
| File transfers | `ssh.rs`, `sftp.rs`, `app.rs`, `Cargo.toml` | `ConflictPolicy`, `TransferProgress`, `SftpAction::{Upload,Download}` → `SshSession::{sftp_upload,sftp_download}`, `upload_items`/`download_items`, `collect_upload`/`collect_download`, `SftpExplorer::{download_entries,upload_paths,upload_into,resolve_conflict,cancel_transfer,transfer_progress,transfer_label,queued_transfers}`, `TransferRequest`, `ConflictPrompt`, `ActiveTransfer`, `RustTermApp::sftp_conflict_ui`, `sftp_transfer_bar`, `rfd` 0.15 (`xdg-portal`,`tokio`) | ✅ done (53 tests both feature sets; clippy/fmt clean; GUI-unverified — checklist in `USAGE.md` §17) |
| Transfer hardening | `ssh.rs`, `sftp.rs`, `app.rs` | `SshSession::{is_alive,alive}`, `SessionGuard`, `compression::NONE`, `CONNECTION_LOST`, `is_connection_loss`, `normalise_transfer_error`, `collect_upload` via `spawn_blocking`, always-close loops, `SftpExplorer::{mark_unavailable_with,mark_closed}`, `ActiveSession::connection_lost`, status-bar progress, `Delete “name”` heading | ✅ done (55 tests both feature sets; clippy/fmt clean; needs live re-test of PDF/folder upload) |
| Next: edit-and-auto-upload | `sftp.rs`, `ssh.rs`, `app.rs` | TBD — watch the temp download folder and re-upload on save | ⏳ pending |
⚙

---

## 17. Usage

Feature behaviour, gestures, keybindings, and the per-feature **manual GUI test
checklist** live in [`USAGE.md`](USAGE.md). Keep that file updated as features
land — it is the acceptance-criteria document, not a changelog.
