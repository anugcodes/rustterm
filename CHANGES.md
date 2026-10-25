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
