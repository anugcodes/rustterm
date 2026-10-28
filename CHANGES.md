# Step: Tabs + F1 (honest connect) + F2 (explicit disconnect)

## What changed

**`src/ssh.rs` — rewritten.**
- `SshSession::connect` is now **synchronous through authentication**: it builds
  the runtime, does TCP connect + password auth + PTY + shell request on the
  caller's thread, and only then spawns the I/O thread. It returns
  `anyhow::Result<Self>`, so the caller sees real errors (F1 fixed).
- `SshSession` gained `disconnect_tx` and a `disconnect()` method. `run_session`
  has a matching `select!` branch that calls `session.disconnect(...)` and exits.
  Dropping the session still works, but every exit path now also disconnects
  gracefully (F2 fixed).
- Added `try_read()` so the app can drain output without touching the receiver
  type directly.

**`src/app.rs` — rewritten.**
- Tabs. Index 0 is the always-present **New Session** tab; sessions live at
  indices 1..N. Top bar, full width.
- One connect attempt at a time. `SshSession::connect` runs on a background
  thread; `poll_connecting` checks the result every frame. Failure shows inline
  in the form — no phantom terminal, no dead `Ok(Err)` branch.
- Per-tab focus reset on tab switch and after closing a tab.
- Closing a tab or pressing Disconnect calls `SshSession::disconnect()`.

## Small additions needed to `src/terminal.rs`

`app.rs` assumes two things about `TerminalView`:

1. **`ui()` returns `egui::Response`.** The handoff's focus discussion implies it
   already does — nothing to change if so.
2. **`grid_size() -> (u16, u16)`** — the current grid dimensions after layout.
   Add:

   ```rust
   impl TerminalView {
       pub fn grid_size(&self) -> (u16, u16) {
           (self.cols, self.rows) // adjust to your field names
       }
   }
   ```

   If your `ui()` already returns the new size (or you track it in a field),
   use that instead and drop this.

## API contracts `app.rs` assumes (verify against your source)

- `TerminalView::new(cols: u16, rows: u16)` — if your constructor takes a font
  size or a config struct, adjust the `TerminalView::new(DEFAULT_COLS, DEFAULT_ROWS)`
  call in `poll_connecting`.
- `TerminalView::feed(&mut self, data: &[u8])` — unchanged.
- `TerminalView::ui(&mut self, ui: &mut egui::Ui) -> egui::Response` — the
  terminal widget must be clickable (or at least focusable) so `request_focus`
  and `has_focus` behave.
- `input::event_to_bytes(event: &egui::Event, mods: egui::Modifiers) -> Option<Vec<u8>>`
  — if it returns `Vec<u8>` directly, replace the `if let Some(bytes)` with a
  `let bytes` + `if !bytes.is_empty()`.
- `SshConfig { host, port, user, password }` — unchanged field set.

## russh 0.45 notes (if anything doesn't compile)

- Channel type is `russh::Channel<russh::client::Msg>`. `channel.wait()` yields
  `Option<russh::ChannelMsg>`.
- `channel.data(...)` takes `impl Into<Vec<u8>>`; a `Vec<u8>` works.
- `channel.window_change(cols: u32, rows: u32, width: u32, height: u32)`.
- `channel.request_pty(want_reply, term, col_width, row_height, pix_width, pix_height, &[(Pty, u32)])`.
- `session.disconnect(russh::Disconnect::ByApplication, "", "en")`.
- `client::Handler::check_server_key` still accepts any key — **F3 is untouched**.
  This is fine for `localhost` only.

## Behaviour notes

- The New Session tab is never closable. `＋ New Session` is the first tab.
- After a successful connect, the new session tab becomes active; host/port/user
  stay in the form (password is cleared) for quick reconnects.
- While a connect is in flight, the Connect button is disabled and a spinner
  shows. Only one attempt at a time.
- Closing the active tab falls back to the New Session tab. Closing a tab to the
  left of the active one shifts the active index down by one.
- `ctx.request_repaint_after(30ms)` keeps output flowing (unchanged design).

## What this step deliberately does NOT do

- No tests (F18), no session persistence, no keyring, no key auth, no SFTP.
- F3 (host key verification) still pending — `check_server_key` returns `Ok(true)`.
- F4/F5/F7/F8 etc. untouched.
- `input.rs`, `terminal.rs` bodies, `main.rs` unchanged.

## Test checklist

1. `cargo build` — zero warnings. `cargo clippy -- -D warnings`.
2. Launch: New Session tab is shown, form is focused.
3. Wrong password → error appears inline in the form, still on New Session tab,
   no phantom terminal. **This is F1 fixed.**
4. Bad host/port → same.
5. Correct login → new tab appears, becomes active, shell prompt renders.
6. Open a second session via `＋ New Session` → two tabs, switch between them,
   input goes to the active one, focus follows.
7. `ls --color`, `nano`, arrows/Home/End/F-keys still work in each tab.
8. Resize the window → remote PTY reflows (per tab).
9. Click `×` on a session tab → tab disappears; `ss -tnp | grep :22` (or
   `loginctl`) shows the connection closed promptly. **This is F2 fixed.**
10. Press Dis

---

# Step: Legacy-server compatibility + interactive performance

## What changed

**`src/ssh.rs`**
- `client_config()` keeps russh's modern defaults first and appends two
  fallbacks required by OpenSSH 5.3: `diffie-hellman-group14-sha1` for KEX and
  `ssh-rsa` for host keys. Modern servers still negotiate the modern entries
  because the client's list order is preserved.
- `known_rsa_key_status()` compares RSA key material against `known_hosts`
  while ignoring the algorithm name. `known_hosts` entries always parse as
  `rsa-sha2-256`, but an old server presents the same key as `ssh-rsa`;
  without this comparison the TOFU prompt reappeared on every connect.
- The TCP socket is created explicitly and `set_nodelay(true)` is applied
  before `client::connect_stream`. On a ~370 ms RTT link, Nagle/delayed-ACK
  added hundreds of milliseconds to each keystroke. OpenSSH sets this itself.
- Compression is enabled: `zlib@openssh.com` is moved to the front of the
  preference list (plain `zlib` retained as a fallback). This matches `ssh -C`;
  russh initialises compression only after authentication.
- `SshSession::connect(config, prompt_tx, wake)` gained a `Wake` callback
  (`Arc<dyn Fn() + Send + Sync>`). `run_session` invokes it after forwarding
  output and on exit.

**`src/app.rs`**
- Replaced the 8 ms polling window / 3-frame tail with an SSH-thread wake that
  calls `egui::Context::request_repaint`. Previously remote echo could remain
  invisible until the next keyboard or mouse event.
- `pump_sessions(ctx)` drains **all** sessions, not just the active tab, so
  background tabs keep their SSH window moving. Titles and bells are also
  processed for every tab.
- Added a 256 KiB/frame decode budget with a self-repaint when work remains, so
  a large burst cannot freeze the UI.
- Repaint timers are now used only while a connection attempt is pending.

**`Cargo.toml`**
- `[profile.dev]` `opt-level = 1`, dependencies `opt-level = 2`.
- `[profile.release]` `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`.

## Measurements

- Terminal decode: 16 MiB in ~0.36 s (44 MiB/s) in the dev profile.
- Compression (local OpenSSH 9.6, 1 MiB): ~56–59 MiB/s compressed vs
  ~133–155 MiB/s uncompressed. It reduces bytes on the high-latency link and
  matches the user's fast `ssh -C` baseline.
- RTT to `172.31.251.23`: min 290 ms, avg 370 ms, max 490 ms.
- That server reached `authentication rejected by server` with a deliberately
  wrong password, proving KEX and host-key verification succeed.

## Verified

`cargo build` ✅, `cargo build --release` ✅, `cargo test` (19) ✅,
`cargo clippy --all-targets -- -D warnings` ✅, `cargo fmt --check` ✅.

---

# Step: Saved session profiles

## What changed

**`src/sessions.rs` — new.**
- `SessionProfile { name, host, port, user }` with `validate()`.
- `SessionStore` loads/saves `$XDG_CONFIG_HOME/rustterm/sessions.toml`
  (falling back to `~/.config/rustterm/sessions.toml`).
- Missing file → empty list, no error. Malformed file → error surfaced to the
  UI, app still starts.
- Writes are atomic (`sessions.toml.tmp` + rename) and create the config
  directory. On a write failure, the in-memory list is left unchanged.
- `upsert()` replaces a profile with the same name and returns its index;
  `remove()` deletes and persists.
- Passwords are deliberately **not** part of a profile (F19 remains open).
- 7 new unit tests cover round-trip, replace-by-name, delete, missing file,
  malformed file, default port, and validation.

**`src/app.rs`**
- New Session tab now shows a Profile name field + **Save profile** button, and
  a scrollable **Saved sessions** list with **Delete selected** and the active
  file path.
- Clicking a profile fills host/port/user (never a password) and highlights it.
- Saving with an existing name updates that profile. A blank name defaults to
  `user@host`.
- Status messages are now typed: green for confirmations, red for errors.
- A repaint is requested after save/delete so the list updates immediately.

**`src/main.rs` / `Cargo.toml`**
- Registered `mod sessions`.
- Added `serde` (derive) and `toml` 0.8.

## Verified

`cargo build` ✅, `cargo test` (26) ✅, `cargo clippy --all-targets -- -D warnings` ✅,
`cargo fmt --check` ✅. GUI behaviour needs the manual checklist in `USAGE.md` §12.

---

# Step: Optional keyring password storage (F19)

## What changed

**`src/secrets.rs` — new.**
- Thin wrapper over `keyring` 4.2: service `rustterm`, account = profile name.
- `save` / `load` / `delete`; a missing entry is `Ok(None)` / a no-op, while a
  locked or unavailable keyring returns a readable error string.
- No panics: a keyring problem always degrades to manual password entry.

**`src/sessions.rs`**
- `SessionProfile` gained `save_password: bool` (`serde(default,
  skip_serializing_if = "is_false")`), so older files load unchanged and
  password-less profiles keep writing a clean file.
- `validate()` now requires an explicit profile name when `save_password` is
  true: `Profile name is required when saving the password`.
- New tests: flag round-trip without writing the secret, legacy files default
  to `false`, `false` is omitted on re-save, and the name requirement.

**`src/app.rs`**
- Password row gained the opt-in checkbox **Save password in the system
  keyring**.
- Saving with the box ticked:
  - refuses a blank profile name (no `user@host` auto-name fallback);
  - refuses an empty password;
  - writes the secret to the keyring *before* the profile, so the TOML never
    claims a stored password when the keyring failed;
  - on write failure, rolls nothing back into the profile file.
- Loading a flagged profile fills the password from the keyring. A missing or
  unreadable entry produces an inline error but the form still works manually.
- Unticking the box and re-saving deletes the stored secret. Deleting a profile
  deletes its keyring entry. Password-less profiles never touch the keyring.
- The profile-name hint changes to "required to save password" while the box is
  ticked.

**`src/main.rs` / `Cargo.toml`**
- Registered `mod secrets`; added `keyring = "4.2"`.

## Verified

`cargo build` ✅, `cargo test` (29) ✅,
`cargo clippy --all-targets -- -D warnings` ✅, `cargo fmt --check` ✅.
A temporary probe against the running GNOME Keyring stored, read, deleted and
re-read a test secret, and a second probe verified the profile + keyring flow
(including no plaintext in the TOML and cleanup on uncheck/delete). GUI
behaviour needs the checklist in `USAGE.md` §12.

---

# Step: Make keyring support an optional Cargo feature

## Why

Measured the cost of `keyring` 4.2: **47 extra crate names** (264 → 311 unique
normal dependencies; `Cargo.lock` has 467 packages) and about **2.3 MB** of
stripped release binary (13.6 MB → 11.2 MB). The code itself is small, but the
zbus/Secret Service stack is heavy for users who never store passwords.

Runtime download/loading of a Rust crate is not possible (no stable plugin
ABI). Two viable packaging choices: a compile-time feature or a separate helper
binary. The feature flag was implemented; the helper binary is deferred until
there is real demand.

## What changed

**`Cargo.toml`**
- Added `[features] default = ["keyring-secrets"]`,
  `keyring-secrets = ["dep:keyring"]`, and made `keyring` optional.
- Normal `cargo build` / `cargo run` is unchanged.

**`src/secrets.rs`**
- Split into `enabled` (real keyring calls) and `disabled` stubs, selected by
  `cfg(feature = "keyring-secrets")`; the public `save`/`load`/`delete` API is
  identical in both builds.
- The stub reports "this build has no keyring support" on save/load and makes
  delete a no-op.

**`src/app.rs`**
- `KEYRING_ENABLED = cfg!(feature = "keyring-secrets")`.
- The keyring checkbox is not drawn in lean builds.
- `save_profile`: keyring save happens only when the feature is enabled and the
  box is ticked; unchecking deletes the secret only in a keyring build.
- In a lean build, re-saving a profile that already has `save_password = true`
  **keeps the flag** instead of silently clearing it, so a later full build can
  still find the secret.
- `load_profile`: a flagged profile in a lean build shows a clear "no keyring
  support, type the password" error rather than failing silently.
- `delete_selected_profile`: only calls the keyring in a keyring build.

## Measured

| Build | Unique normal deps | Stripped release binary |
|---|---|---|
| default (`keyring-secrets`) | 311 | 13.6 MB |
| `--no-default-features` | 264 | 11.2 MB |

`strings` on the lean binary finds no zbus/Secret Service/keyring symbols, only
the app's own UI messages.

## Verified

- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅ (both feature sets)
- `cargo test` ✅ **29 passed** (both feature sets)
- `cargo build --release` ✅ and `cargo build --release --no-default-features` ✅
- Default release rebuilt last, so `target/release/rustterm` is the normal build.

## Recommendation

Keep `keyring-secrets` default-on and document
`cargo build --release --no-default-features` for minimal packaging. Users who
want the feature later install a build that includes it; do not attempt runtime
dlopen of crates.

---

# Step: Public-key and ssh-agent authentication

## What changed

**`src/ssh.rs`**
- `SshConfig` gained `use_key`, `key_path` and `use_agent`.
- New `authenticate()` runs methods in the order users expect:
  1. private key file(s) — explicit `key_path`, or auto-detect
     `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`, `id_dsa` when blank;
  2. every identity in `$SSH_AUTH_SOCK`, reusing one agent connection via
     `authenticate_future`;
  3. the typed password (fallback, or sole method).
- `~` / `~/…` expansion for the key path.
- Failures are collected per method and reported together. The message never
  contains the password. Encrypted keys get a specific hint: `ssh-add` + enable
  the agent option.
- `AgentClient::connect_env()` errors (agent missing/not running, no identities)
  are surfaced clearly.

**`src/app.rs`**
- New form rows: **Public key** (Use checkbox + path, blank = auto-detect) and
  **SSH agent**.
- `start_connect` refuses to start with no password and no key/agent method.
- Profile load/save round-trips the three new fields.

**`src/sessions.rs`**
- `SessionProfile` gained `use_key`, `key_path` and `use_agent`, all with serde
  defaults and omitted when off/empty, so older TOML files load unchanged.
- Tests: key settings round-trip, blank `key_path` omitted, old files default to
  password-only.

## Verified

- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅ (default and
  `--no-default-features`)
- `cargo test` ✅ **31 passed** (default and `--no-default-features`)
- `cargo build --release` ✅
- End-to-end probe against a temporary russh SSH server on loopback:
  - explicit Ed25519 key file → **FILE_KEY_OK**
  - identity loaded in a real `ssh-agent` (`SSH_AUTH_SOCK`) → **AGENT_OK**
  - password fallback → **PASSWORD_OK**
  - passphrase-protected key → clear inline error, no hang/crash.

## Known limits (documented in handoff F23/F24)

- Passphrase-protected key files are not decrypted in-app; use ssh-agent.
- russh-keys 0.45 has no SHA-1 RSA signing path, so OpenSSH 5.3 servers that
  only offer `ssh-rsa` will reject key auth. Password auth to those hosts still
  works (the legacy target uses a password).
- GUI checklist still needs a manual pass (`USAGE.md` §13).

---

# Step: SFTP side panel

## What changed

**`Cargo.toml` / `Cargo.lock`**
- Added `russh-sftp = "3.0"` (resolved to 3.0.1, same `russh` 0.45 line).

**`src/ssh.rs`**
- `connect_and_auth` now also opens a second session channel, requests the
  `sftp` subsystem (`request_subsystem(true, "sftp")`), and builds an
  `SftpSession` with a 10 s request timeout. SFTP failure is **non-fatal**: the
  terminal connects, and `sftp_unavailable()` carries the reason for the panel.
- `SshSession` gained `sftp_home()` / `sftp_list(path)`, both returning
  `std::sync::mpsc::Receiver`s. A `Wake` callback fires when a reply is ready so
  the panel repaints without polling.
- `run_session` gained an SFTP request branch and `SessionIo` (a small struct for
  the channel receivers/output sender, keeping clippy's argument limit happy).
  Each request is `tokio::spawn`ed, so a slow `readdir` never blocks terminal
  input/output.
- New `RemoteEntry` (`name`, `path`, `is_dir`, `is_symlink`, `size`, `modified`)
  filled from `DirEntry::metadata()`.

**`src/sftp.rs` (new)**
- `SftpExplorer`: current/editable path, entries, selection, loading/error/
  unavailable state, and one in-flight `Job` (`Home` or `List`).
- `start` resolves the home directory; `poll` drains the reply and starts the
  first listing; `navigate`, `refresh`, `up`, `load_home` drive the panel.
- `join_path`, `parent_path`, `sort_entries` (directories first, then
  case-insensitive name), `format_size`, `format_age`.
- Five unit tests for those helpers.

**`src/app.rs`**
- `ActiveSession` owns an `SftpExplorer`; `RustTermApp` owns `show_sftp`
  (default on). `SftpExplorer::start(&ssh)` runs before `ssh` is moved into the
  session.
- `pump_sessions` polls SFTP for **every** tab (so a background tab's request
  cannot stall) and feeds the result into the repaint flag.
- A resizable `SidePanel::left("sftp_panel")` is drawn for the active session
  when `show_sftp` is on: Home/Up/Refresh, path box + Enter/Go, spinner, error
  text, scrollable listing, and a selected-path/size/age footer.
- **Files** toggle in the tab bar (only when a session is active) and a **×** in
  the panel hide it. Nothing blocks the UI thread.

**`src/main.rs`**
- Registered `mod sftp;`.

## API findings

- `russh-sftp` 3.0's `read_dir` loops `READDIR` until `StatusCode::Eof`. Its
  server trait uses RPITIT (`fn … -> impl Future + Send`) unless the crate's
  `async-trait` feature is enabled; the older russh-sftp server example that
  still uses `#[async_trait]` does not compile against 3.0.
- `SftpSession` is not `Clone`; requests run on `Arc<SftpSession>` inside the
  session runtime.
- `Metadata::modified()` returns `io::Result<SystemTime>` (not an `Option`), and
  `FileType` exposes `is_dir()` / `is_symlink()`.

## Verified

- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅
- `cargo test` ✅ **36 passed**
- End-to-end probe against a temporary in-process russh server with an SFTP
  subsystem (probe deleted afterwards): `canonicalize(".")` → `/probe`;
  `read_dir` returned `docs` (dir, 4096, mtime present) and `readme.txt` (file,
  1234, mtime present) in the expected order.
- GUI checklist still needs a manual pass against a real server (`USAGE.md` §14).

## Known limits (F26)

- The panel starts at the login home and does **not** follow shell `cd`; OSC 7
  is not parsed and alacritty_terminal 0.24 does not expose the current
  directory. Manual navigation only for now.
- No upload/download yet; symlinks are displayed but not followed.
- A server that permits only one session channel makes SFTP unavailable while
  the terminal continues to work.

---

# Step: SFTP panel follow-ups

## What changed

**`Cargo.toml` / `Cargo.lock`**
- Added `chrono = "0.4"` for local-time formatting. It was already in the lock
  file as a transitive dependency; this makes the use explicit.

**`src/ssh.rs`**
- `RemoteEntry` gained `owner`.
- Introduced `SftpClient { session, owners }`, where `owners` is a
  `tokio::sync::OnceCell<HashMap<u32, String>>` filled from `/etc/passwd` on the
  first listing. Owner precedence: cached name → `Metadata.user` → numeric UID.
- Replaced the optional `Arc<SftpSession>` with `SharedSftp`
  (`Arc<Mutex<Option<Arc<SftpClient>>>>`) and wrapped the russh
  `Handle<ClientHandler>` in an `Arc` (the handle is not `Clone`).
- Added `SftpAction::{Reconnect,Rename,Delete,CreateDir,Copy,Fetch}` and
  `SshSession::{sftp_reconnect,sftp_rename,sftp_delete,sftp_create_dir,
  sftp_copy,sftp_fetch}`. All replies are `std::sync::mpsc::Receiver`s and wake
  egui.
- Reconnect is dispatched specially in `run_session`: it opens a second channel,
  requests `sftp`, builds a new client, swaps the shared slot and closes the old
  channel. The terminal channel is untouched. Every other action clones the
  current client and runs in a spawned task.
- File operations: recursive lstat-based delete, unique `name (copy N)`
  destinations, recursive copy with `tokio::io::copy` for files, symlink
  recreation, partial-destination cleanup, and single-file download to a local
  path for the editor.
- Added `parse_passwd`/owner tests and remote-path-join tests.

**`src/sftp.rs`**
- `SftpExplorer` gained `show_hidden`, `clipboard`, `dialog`, `notice`, an
  operation receiver and reconnect jobs.
- Added `reconnect`, `copy_entry`, `paste`, `paste_into`, `begin_rename`,
  `begin_delete`, `begin_new_folder{,_in}`, `submit_dialog`, `cancel_dialog` and
  `open_in_editor`.
- Added `SftpDialog`, `SftpDialogKind`, `ClipboardEntry`, `EditorChoice`,
  `format_timestamp`, `format_owner`, `validate_name`, copy-target guards,
  unique temp download paths and editor command resolution.
- Editor launch tries `$VISUAL`, then `$EDITOR`, then `xdg-open`; explicit
  choices are Zed, gvim, VS Code and the desktop default. The child is reaped on
  a background thread so the UI never blocks.
- Added six unit tests (timestamps, names, copy guard, editor commands).

**`src/app.rs`**
- SFTP toolbar now has **New folder**, **Copy**, **Paste**, **Hidden**,
  **Reconnect** and an operation spinner/label.
- Rows show `icon name` plus a second line with owner and local modified time;
  the footer repeats owner/time and adds size, with **Rename**/**Delete**
  shortcuts.
- Right-click row menu: Open, Open in default editor, Open in editor submenu,
  Rename…, Copy, Copy path, Paste into this folder, New folder…, Delete…,
  Refresh.
- Rename / new-folder / delete dialogs are rendered as anchored egui windows;
  delete confirms recursively for directories.
- Unavailable state offers **Reconnect SFTP** without touching the terminal.

**`handoff.md` / `USAGE.md` / `README.md`**
- Roadmap step 7 marked done; transfers are now next. README status and feature
  checklist refreshed.
- API notes updated (`Arc<Handle>` not `Clone`, `/etc/passwd` cache, symlink
  argument order, `File` streaming/close).
- §14 checklist expanded for columns, hidden files, dialogs, copy/paste,
  symlinks, editor launch, reconnect and responsive large operations.

## Verified

- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅ (default and
  `--no-default-features`)
- `cargo test` ✅ **42 passed** in both feature sets
- `cargo build --release` ✅
- GUI checklist for the new operations still needs a manual live-server pass
  (`USAGE.md` §14).

## Known limits

- No general upload/download, progress bars, transfer queue or drag & drop yet;
  **Download…/Upload here…** stay with the transfers step.
- Remote copy/paste has no server-side primitive, so it streams bytes through
  the client. Very large trees are slow by nature but do not block the UI.
- Panel keyboard `Ctrl+C`/`Ctrl+V` is intentionally unbound to avoid stealing
  terminal shortcuts; use the toolbar or context menu.
- Symlinks are shown and copied but not navigated. The panel still does not
  follow shell `cd`.
- Editor changes are not watched or uploaded; close/reopen after editing.

# Step: UI overhaul (SFTP table, status bar, New Session, themes)

## What changed

**`Cargo.toml` / `Cargo.lock`**
- Added `egui_extras` 0.29 (default features off) for `TableBuilder`.

**`src/theme.rs` (new)**
- Theme registry with `ThemeId::{Midnight, Graphite}`, `Palette` and `Metrics`
  tokens, and `apply()` which installs the full egui style (visuals, widget
  states, spacing, rounding, fonts, scrollbars, shadows) globally.
- Helpers used by the screens: `card_frame`, `modal_frame`, `callout_frame`,
  `section_label`, `muted`, `icon_button`, `icon_toggle`.

**`src/settings.rs` (new)**
- `Settings { theme }` loaded from and saved atomically to
  `$XDG_CONFIG_HOME/rustterm/settings.toml` (or `~/.config/rustterm/…`).
  Missing/unknown theme falls back to Midnight.

**`src/main.rs`**
- Registers the new modules and applies the saved theme in
  `RustTermApp::new`.

**`src/app.rs`**
- SFTP listing is a `TableBuilder` with a sticky **Name / Owner / Modified /
  Size** header, resizable/elided columns, striping and a 420 px default /
  340 px minimum panel. Single click selects, double click opens, right-click
  selects and opens the menu.
- Context menu trimmed to Open / Open with… / Paste into this folder /
  Rename… / Copy path / Delete… (no Copy, New folder… or Refresh).
- Icon toolbar with tooltips (🏠 ⬆ 🔄 ➕ 🗐 📋 👁 ⚡ ×) and a ➡ path button.
- Bottom status bar: latest message with severity colour, spinner/operation
  label while busy, ☰ history popover (last 50 entries) and Clear. The old
  selected-file footer and inline notice/error block are removed.
- **Open with…** modal (Default editor, Zed, gvim, VS Code, Desktop default,
  Custom command) with disabled unavailable editors and a remembered choice.
- Rename / New folder / Delete share `theme::modal_frame`; Delete shows the
  path and a tinted warning for recursive folder removal. `Esc` cancels.
- New Session page rebuilt as a centred column of Connection, Profile and
  Saved sessions cards with a primary Connect button and in-flight spinner.
- Theme picker pinned to the far-right of the tab bar; selection applies
  immediately and persists.
- Replaced font-unsupported glyphs (`＋ ⌂ ↑ → ⧉ ◉ ◐ ✓ ● ⋯`) with ones present
  in egui's bundled fonts (`+ 🏠 ⬆ ➡ 🗐 👁 ✔ • ☰ 🎨`) so nothing renders as tofu.

**`src/sftp.rs`**
- `StatusLog`/`StatusEntry`/`StatusKind` replace `notice`/`error`; every listing,
  operation, reconnect and editor launch reports to it (bounded at 50).
- `EditorPicker` + `begin_open_with` / `submit_open_with` / `cancel_open_with`;
  `EditorChoice::Custom`, command parsing, `editor_program` and
  `command_available` for the picker.
- Automatic refreshes no longer overwrite a success message from the operation
  that triggered them.

**Docs**
- `README.md`, `USAGE.md` (§14 rewritten, new §15 themes and §16 New Session),
  `handoff.md` (layout, status, gotchas, tracker) and `PLAN-ui-overhaul.md`.

## Verified

- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅ (default and
  `--no-default-features`)
- `cargo test` ✅ **48 passed** in both feature sets
- `cargo build --release` ✅
- On-screen checks: New Session layout at 1600×1000 and narrow widths, tab bar
  with the theme picker pinned right, Midnight ↔ Graphite switching, persisted
  theme across restart.

## Known limits

- Live-SFTP GUI verification (table columns at 340 px, right-click menu,
  status history while copying, editor picker launch, delete dialog) still needs
  a server with credentials; the code paths are unit-tested and the checklist is
  in `USAGE.md` §14.
- The table sorts directories first by name (as before); clicking headers does
  not change the sort yet.
- Themes style the application chrome only; the terminal ANSI palette is
  unchanged.

# Step: Flat, boxy UI refinement pass

## Why

The first UI pass used `egui_extras` and soft egui defaults (rounded corners,
shadowed popups, hover glow, loose padding). The result wasted vertical space,
the SFTP panel could grow over the terminal, resize handles appeared on every
row, and the New Session page spread one task over three cards. This pass makes
the UI flat, compact and predictable.

## What changed

**`Cargo.toml` / `Cargo.lock`**
- Removed `egui_extras`; the SFTP table is hand-rolled.

**`src/theme.rs`**
- Rebuilt the palette/metrics for a flat design: `Rounding::ZERO`,
  `Shadow::NONE`, 1 px borders, hover changes fill only (no accent glow), a
  shared 26 px control height, one padding scale, `Selection.stroke` in the text
  colour and thin floating scrollbars.
- Added `body_font`, `panel_frame`, `primary_button` and `danger_button`
  helpers; removed `small_font` and `accent_hover`.

**`src/app.rs`**
- **SFTP panel**: default 450 px, minimum 340 px, maximum
  `min(55% of the window, 720 px)`, still draggable. The terminal is never
  covered, including at 900 px window width.
- **SFTP table**: fixed header painted over a `ScrollArea::show_rows` body.
  Column dividers exist only in the header and resize the column to their right.
  Header and file names use the same size and weight (no bold, no "Remote
  files" title). Icons are `🗀` / `🗋` / `🔗` at body font size; defaults are
  owner 64, modified 122, size 52 and an 80 px name minimum.
- **Inputs**: one shared helper applies `vertical_align(Center)`, a `(pad, 2)`
  margin and a 26 px height, so form text sits on one baseline.
- **Tabs**: single box per tab with the `×` inside it and a 168 px minimum;
  middle-click also closes.
- **New Session**: connection fields, auth options, the **Save as profile**
  checkbox (which enables the name field) and Connect now live in one card.
  Connect saves the profile first and aborts on validation failure. The saved
  list is a compact **Saved profiles (n)** table with **Name / Target**,
  **Load** and a per-row **×**. The page is narrower and scrolls at small
  window sizes.
- **Dialogs**: shared `theme::modal_frame` with custom title bars
  (`.title_bar(false)` for the SFTP dialogs) and `.default_size(vec2(width,
  1.0))` so they fit their content. `.auto_sized()` was tried and reverted
  because egui expands it to the whole area. The host-key prompt was restyled to
  match (framed fingerprint, right-aligned Reject / Accept & save).
- **Theme menu**: just the two themes; it closes on selection and the developer
  hint is gone.
- **Terminal**: the Disconnect button moved to a bottom panel declared before
  the terminal so it cannot be clipped.

## Verified

- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅ (default and
  `--no-default-features`)
- `cargo test` ✅ **48 passed** in both feature sets
- `cargo build --release` ✅
- On-screen, against a local sshd on `127.0.0.1:2222`: panel cap/default/minimum,
  hide/show keeping width/path/selection, header-divider resize, full
  timestamps, new icons, in-tab close `×`, terminal reflow when the panel is
  hidden, bottom Disconnect bar, New Session at 900×700 and 700×400,
  Midnight ↔ Graphite switching and persistence across restart.
- Dialogs: Rename, New folder, Delete and **Open with…**; host-key prompt with
  Reject (connection refused) and Accept & save (key written to `known_hosts`,
  connection succeeds).

## Known limits

- Headers are not clickable; the table keeps the directories-first name sort.
- Very narrow windows (< 900 px) with the panel open were not exercised.
- Theme switching still only affects the chrome, not terminal ANSI colours.

# Step: File transfers (upload, download, queue, drag & drop)

## What changed

**`src/ssh.rs` — transfer backend.**
- `ConflictPolicy { Overwrite, KeepBoth }` and `TransferProgress`: an atomic
  handle for bytes done/total, files done/total and cancellation, so the UI can
  render progress every frame without a second channel.
- `SftpAction::{Upload,Download}` and the public
  `SshSession::{sftp_upload,sftp_download}`. Both use the usual one-shot
  `std_mpsc` reply, so completion flows through `SftpExplorer::poll` like every
  other SFTP request.
- Recursive pre-walks (`collect_upload`, `collect_download`) compute the byte and
  file totals before the first chunk; this is what makes a real progress bar
  possible. Directory trees and symlinks are recreated in both directions.
- Files stream in 64 KiB chunks. Every file is written to `<name>.part` and
  renamed only on success; cancellation or an error removes the partial file.
  Uploads retry the rename once after removing the target, because not every
  server's SFTP rename overwrites.
- Per-item failures are collected and summarised instead of aborting the whole
  batch; cancellation is checked between chunks and between items.

**`src/sftp.rs` — queue and conflict state.**
- `TransferRequest` / `TransferKind` describe queued work; `ActiveTransfer`
  holds the running progress handle. One transfer runs at a time, the rest wait
  in a `VecDeque`.
- `download_entries`, `upload_paths` and `upload_into` detect top-level
  destinations that already exist and raise `ConflictPrompt` before writing.
  `resolve_conflict` applies **Replace** or **Keep both** (or cancels) and starts
  the queue; `cancel_transfer` cancels the running job or drops the queue.
- `poll` now drains transfer results first, refreshes the listing on success and
  starts the next queued job. `is_busy`/`busy_label` account for transfers.

**`src/app.rs` — UI.**
- ⬇ / ⬆ toolbar buttons (selected entry → folder picker; files → upload picker).
- Row menu: **Download…** on files and folders; folders also get **Upload files
  here…** and **Upload folder here…**. All entries use the shared menu helpers.
- A transfer bar above the status bar: label, byte bar, `done / total`, files
  done/total, **×** cancel and `+N queued`. Frames are requested at 100 ms while
  a transfer runs.
- **Items already exist** modal (standard `theme::DIALOG_WIDTH`): lists up to
  five names and offers **Replace / Keep both / Cancel**; `Esc` cancels.
- Desktop drag & drop uploads dropped paths into the visible remote directory.
- `rfd` 0.15 provides the native folder/file pickers. The `xdg-portal` feature
  requires an async backend, so `tokio` is enabled too.

**Fixed while testing:** `unique_local_path` always returned `name (copy)` even
when the original name was free; it now tries the plain name first.

## Tests

- `TransferProgress`: byte/fraction arithmetic including clamping, item counts,
  cancel flag.
- `remote_file_name`, `summarise_failures`, and `unique_local_path` (plain,
  `(copy)`, `(copy 2)`) using a temporary directory.

## Verified

- `cargo build` ✅ (default and `--no-default-features`)
- `cargo clippy --all-targets -- -D warnings` ✅ (both feature sets)
- `cargo test` ✅ **53 passed** (both feature sets)
- `cargo fmt --check` ✅
- GUI verification against a local sshd is still pending; the checklist is in
  `USAGE.md` §17.

# Step: Upload stability, connection-loss handling and transfer UI

## Why

Uploading a PDF or a folder could end with the tab silently dead and the status
bar saying `sender dropped`. The connection was actually being closed by the
server: `russh 0.45.0`'s outbound zlib compressor stops as soon as `flate2`
returns `Ok`, even if the output buffer filled before the input was consumed, so
the tail of an incompressible packet (PDF, image, archive) was dropped. Text
files survived because their compressed output fit. The SFTP request map then
reported `sender dropped` when its transport died. Downloads were unaffected
because compression happens on the server side.

## What changed

**`src/ssh.rs`.**
- Advertise `compression::NONE`. The broken compressor can no longer be
  negotiated; a unit test locks the preference in. Re-enabling compression is
  deferred to a russh upgrade (≥ 0.65 fixes the bug; `russh-sftp` does not
  depend on russh, so the upgrade is independent).
- `SshSession::is_alive()` plus a `SessionGuard` that clears the flag when the
  session thread exits or panics.
- `CONNECTION_LOST`, `is_connection_loss()` and `normalise_transfer_error()`:
  library strings such as `sender dropped` become one clear message, and a dead
  transport aborts the batch instead of failing every remaining item.
- `collect_upload` runs on `spawn_blocking`, so walking a large local tree no
  longer stalls the session's current-thread runtime (terminal input stayed
  frozen during the walk before).
- Upload and download loops now close the remote handle on every path
  (cancellation, read/write error, success), avoiding leaked server descriptors.

**`src/sftp.rs`.**
- `mark_unavailable_with()` for an explicit reason and `mark_closed()` for a
  dead session. On connection loss the queue is dropped, the active transfer is
  cleared and the panel stops offering ordinary recovery.

**`src/app.rs`.**
- `ActiveSession::connection_lost`, set once by `pump_sessions` when
  `is_alive()` turns false. The bottom bar then shows
  **Connection lost — the server closed this session** with **Close tab**;
  scrollback is preserved until the user closes the tab.
- Transfer progress moved into the single SFTP status bar. Fixed widgets are
  reserved first and the label truncates into the remainder, so `+N` can no
  longer overlap the progress text. The separate second bottom panel is gone.
- The delete dialog heading is now `Delete “name”` (truncated with a tooltip)
  and the duplicated body heading was removed.

## Verified

- `cargo build` ✅ (default and `--no-default-features`)
- `cargo clippy --all-targets -- -D warnings` ✅
- `cargo test` ✅ **55 passed** (both feature sets)
- `cargo fmt --check` ✅
- Live re-test still needed: large PDF upload, mixed folder upload, and the
  dead-session banner. Checklists: `USAGE.md` §17, `TESTING.md` §2.11.
