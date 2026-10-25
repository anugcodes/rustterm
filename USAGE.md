# USAGE — rustterm

Living reference for **how each feature works** and **how to verify it by hand**.

Update this file whenever a feature's behaviour, gesture, or keybinding changes.
Keep the checkbox tests in sync with reality — they are the acceptance criteria.

**Legend**

- **[V]** — behaviour confirmed by hand at runtime.
- **[U]** — implemented and compiles, but **not yet hand-verified**.

## Running it

```bash
cd ~/Desktop/VsTerm
cargo run
RUST_LOG=info cargo run    # logs font loading, tab-title changes, bell, etc.
```

Test server setup: see `handoff.md` §5 (disposable `sshuser@localhost:22`).

---

## 1. Tabs and sessions — [V]

**How:** `＋ New Session` is tab 0 and never closes. Fill Host/Port/User/Password
and press **Connect**. Each successful connect opens a new tab to its right.
Click a tab label to switch; `×` closes the tab and disconnects its SSH cleanly.

**Verify**

- [ ] Connect twice → two tabs, each with independent output.
- [ ] Close one → its socket drops: `ss -tnp | grep ':22'`.
- [ ] Idle CPU stays ~0% (repaint gating) — check with `top`.

## 2. Host-key verification (TOFU) — [V]

**How:** connecting to an unknown host shows a modal with the fingerprint.
**Accept & save** appends the key to `~/.ssh/known_hosts`; **Reject** aborts.
A *changed* key is a hard failure with no bypass.

**Verify**

- [ ] Connect to a new host → modal appears; `~/.ssh/known_hosts` grows.
- [ ] Edit the stored key → the next connect fails.

## 3. Scrollback — [V]

**How:** mouse wheel over the terminal scrolls history; `Shift+PgUp` / `Shift+PgDn`
page; `Shift+Home` / `Shift+End` jump to top/bottom. Full-screen apps (`vim`,
`htop`) ignore it, as intended.

**Verify**

- [ ] `seq 1 500`, scroll up → a scrollbar thumb appears on the right edge.
- [ ] Open `vim`, try to scroll → the view does not move.

## 4. Copy / paste — [U]

**Gestures**

| Action | Gesture |
|---|---|
| Select text | drag with the left mouse button (translucent blue highlight) |
| Copy selection | `Ctrl+Shift+C` **or** right-click → **Copy** |
| Paste | `Ctrl+Shift+V` **or** right-click → **Paste** |
| Select everything on screen | right-click → **Select All** |
| Clear the selection | right-click → **Clear selection** |
| Interrupt (unchanged) | bare `Ctrl+C` → sends `ETX` |
| Literal `^V` (unchanged) | bare `Ctrl+V` → sends `0x16` |

**Right-click menu:** opens on the secondary mouse button with **Copy**,
**Paste**, **Select All**, and **Clear selection**. Copy / Clear are greyed out
when there is no selection. It is written as an extensible menu — **Find** and
friends are planned follow-ups.

**Alt-screen:** because alacritty swaps the active grid, `renderable_content()`
(and therefore selection + copy) already reads whatever is on screen — so
copy works inside `vim`, `htop`, and `tmux`. The selection is dropped when the
screen buffer flips (entering/leaving an alt-screen app) to avoid stale
highlights.

**Known limits**

- No double-click word or triple-click line selection.
- A selection scrolled fully off-screen copies only the visible remainder.
- Bracketed paste (flaw **F15**) is not implemented, so pasting multi-line text
  into a shell executes it line by line, and into `vim` may trigger auto-indent.
- Bare `Ctrl+C` vs `Ctrl+Shift+C` cannot be told apart from the egui *event*
  (egui-winit maps both to `Event::Copy`); the decision uses the frame's current
  `Modifiers` (`input::is_copy_gesture`). This is the fragility tracked as
  **F16**.

**Verify — copy**

```bash
sudo apt install -y xclip      # once
```
- [ ] In the terminal run `echo hello world`.
- [ ] Drag-select `hello world` → blue highlight appears.
- [ ] `Ctrl+Shift+C`, then in another terminal: `xclip -o -selection clipboard`
      → prints `hello world`. (Or paste into another tab's Host field.)
- [ ] Right-click → **Copy** does the same.
- [ ] Start `sleep 100`, select some text, press bare `Ctrl+C` → it **interrupts**
      (does not copy).

**Verify — paste**

- [ ] Locally: `echo -n 'ls -la' | xclip -selection clipboard`.
- [ ] In rustterm press `Ctrl+Shift+V` → `ls -la` appears at the prompt but does
      **not** run.
- [ ] Repeat via right-click → **Paste**.
- [ ] Bare `Ctrl+V` sends `^V` (no paste).

**Verify — menu + select all**

- [ ] Right-click with no selection → Copy and Clear selection are greyed out.
- [ ] Right-click → **Select All** → the whole screen highlights; copy it.
- [ ] Right-click → **Clear selection** → highlight disappears.

**Verify — alt-screen**

- [ ] Run `vim`, drag-select some text → highlight appears; copy it.
- [ ] Paste a single line into `vim` insert mode → it appears as text.
- [ ] Exit `vim` → the selection is cleared.

## 5. Resize — [V]

**How:** drag the window edge; the grid reflows and the remote PTY receives a
`SIGWINCH`.

**Verify**

- [ ] `stty size` before/after a resize changes.
- [ ] Resize while `htop` is open → it reflows.

## 6. Tab title — [V]

**How:** the remote sets the title via OSC 0/2.

**Verify**

- [ ] `printf '\033]0;My Title\007'` → the tab label becomes `My Title`.

## 7. Bell — [V]

**How:** the bell is logged only; there is no audible beep yet.

**Verify**

- [ ] `printf '\a'` with `RUST_LOG=info` → the log prints `terminal bell`.

## 8. Colours (backgrounds, inverse, 256-colour) — [U]

**How:** per-cell backgrounds come from `cell.bg`, or `cell.fg` when
`Flags::INVERSE` is set. Backgrounds are merged into contiguous runs by
`bg_runs()` (unit-tested). Reverse video with default colours maps to the dark
default background + light default foreground, so text stays readable.
256-colour uses the standard xterm cube + grey ramp (flaw **F11** still open for
remote palettes).

**Known limits:** no bold/dim font weight, no underline/strikethrough,
`Flags::HIDDEN` renders as blank. (Mouse reporting is §9.)

**Verify — regression checks (these were broken, must now be correct)**

- [ ] **nano**: run `nano`; the bottom shortcut bar shows the keys (`^G`, `^O`,
      …) as small highlighted boxes with **readable** text — no giant white bar,
      and no invisible white-on-white text.
- [ ] **htop**: run `htop`; in the bottom `F1Help F2Setup …` bar each key has a
      background that covers **only** its own text, not the neighbouring cells.
      The CPU/memory meters and the header row colour correctly.
- [ ] **tmux**: `tmux` status bar is one solid band (no bleeding past its cells).
- [ ] **vim**: open `vim`, `:set number`, `:syntax on`; `:help` shows coloured
      box-drawing/menus and a visible reverse-video status line.
- [ ] Select text inside `htop`/`tmux` → the blue selection stays **visible**
      over the coloured cells (it is drawn above backgrounds).

**Verify — general**

- [ ] `ls --color=auto` shows the expected file-type colours.
- [ ] `printf '\033[48;5;208m orange \033[0m\n'` shows an orange background.
- [ ] `printf '\033[7m reverse \033[0m\n'` shows readable reversed text.
- [ ] The 24-step grey ramp is smooth:
      `for i in $(seq 232 255); do printf "\033[48;5;${i}m "; done; printf '\033[0m\n'`

## 9. Mouse reporting — [U]

**How:** when the remote program enables xterm mouse reporting (`?1000` press /
`?1002` drag / `?1003` motion, and SGR `?1006`), rustterm forwards your pointer
to it as mouse escape sequences, and the remote app handles clicks, drags and the
wheel itself. When reporting is off, the mouse stays local (selection/scrollback).

The wheel is encoded as button **64** (up) / **65** (down), which is what xterm
does — the bare 4/5 would be read by ncurses as a left/middle *click*, so the
wheel would select rows instead of scrolling.

**Gestures**

| Context | Gesture | Result |
|---|---|---|
| App has mouse mode on | left click / drag | sent to the app (htop row select, header sort, vim visual select, tmux pane focus) |
| App has mouse mode on | wheel | sent to the app (htop scrolls its list) |
| App has mouse mode on | **Shift** + click / drag / wheel | local: text selection / scrollback |
| Any time | right click | local context menu (Copy / Paste / …) |

**Known limits:** UTF-8 mouse coordinates (`?1005`) are not implemented (SGR
covers every modern app); focus in/out reporting (`?1004`) is not implemented.

**Verify**

- [ ] `htop`: click a row → it selects; click the **Main** / **I/O** tabs → they
      switch; click a column header (`PID`, `USER`, `CPU%`) → it sorts; the wheel
      scrolls the process list.
- [ ] `vim`: `:set mouse=a`, then click to place the cursor, drag to select, and
      the wheel to scroll.
- [ ] `tmux`: clicking a pane focuses it; the wheel scrolls its history.
- [ ] **Shift + drag** in htop/vim selects text locally (blue highlight) and
      `Ctrl+Shift+C` copies it.
- [ ] Right click in htop still opens the rustterm context menu.
- [ ] In a plain shell (no mouse mode): left-drag still selects text and the
      wheel still scrolls the local scrollback.

## 10. Keyboard modes (DECCKM) — [U]

**How:** programs can switch the terminal between *normal* cursor keys
(`ESC [ A`) and *application* cursor keys (`ESC O A`) with `?1h` / `?1l`
(DECCKM). rustterm honours that mode: when the program enables it, the arrow,
Home and End keys are sent as SS3 (`ESC O A`); otherwise as CSI (`ESC [ A`).

This matters for every ncurses program. `htop`, `vim`, `mc`, `fzf` etc. turn
`?1h` on through terminfo's `smkx` capability, and ncurses' key parser only
recognises the SS3 sequences — when it receives `ESC [ A` it sees a stray
Escape key followed by the literal characters `[` and `A`, so the arrow keys
appear to do nothing (or trigger random commands).

**Verify**

- [ ] `htop`: **Up/Down** move the highlighted process; **Left/Right** scroll
      the process list horizontally (no setup menu popping up).
- [ ] `htop`: click a process, then press **Up/Down** → the highlight moves to
      the previous/next row (it must not change the process's NI value).
- [ ] `htop`: press **F2** → Setup opens; inside it **Up/Down** move between
      categories/options, **Left/Right** switch panels, **Space** toggles a
      checkbox, **Enter** applies and closes.
- [ ] `vim`: arrow keys move the cursor; `j`/`k` still work.
- [ ] Plain shell: the **Up** arrow recalls the previous command (readline
      accepts both forms, so this must keep working).
- [ ] After quitting `htop`/`vim`, arrow keys in the shell still work — the
      mode must be cleared when the program exits (`?1l`).

## 11. Connection performance and legacy servers — [U]

**How:** every connection enables `TCP_NODELAY` and negotiates OpenSSH's
delayed `zlib@openssh.com` compression (the same as `ssh -C`). The SSH thread
wakes the UI as soon as output arrives, so remote echo is drawn immediately
rather than on the next input event. All tabs are drained every frame, so a
background session never accumulates a backlog.

Old servers remain supported: modern KEX and host-key algorithms are still
preferred, with `diffie-hellman-group14-sha1` and `ssh-rsa` appended as
fallbacks. RSA `known_hosts` entries are matched by key material, so an old
`ssh-rsa` key does not trigger a fresh trust prompt on every connect.

`cargo run` itself is now optimised: local crates build at `opt-level = 1` and
dependencies at `opt-level = 2`. For the best throughput use
`cargo run --release`.

**Verify**

- [ ] Type in a shell on a high-latency host: characters echo back promptly
      (roughly one network round trip), not only after the next keypress.
- [ ] Hold a key down or run `seq 1 300`: output scrolls smoothly and the
      window stays responsive.
- [ ] `cat` a large file and press `Ctrl+C`: the UI reacts and the transfer
      stops; the window does not freeze while output is being decoded.
- [ ] Open two sessions in separate tabs; run a long output in the background
      tab, switch to it → output is already up to date rather than dumped in one
      visible burst.
- [ ] Connect to an OpenSSH 5.x host (e.g. `172.31.251.23`): no "no common key
      exchange algorithm" error, and only one host-key prompt on first use.
- [ ] Idle CPU still stays ~0% while connected.

## 12. Saved session profiles — [U]

**How:** on the **New Session** tab, fill in Host/Port/User, optionally type a
**Profile name** (leave it blank to use `user@host`), and press **Save profile**.
The profile appears in the list below. Click an entry to load its host, port and
user back into the form, then type the password and press **Connect**. Select an
entry and press **Delete selected** to remove it.

Profiles are stored atomically in:

```
$XDG_CONFIG_HOME/rustterm/sessions.toml      # if XDG_CONFIG_HOME is set
~/.config/rustterm/sessions.toml             # otherwise
```

Only connection metadata is written to that file. Saving a profile whose name
already exists updates it in place. Load/save problems are shown inline
(red = error, green = confirmation) and never stop the app from starting.

### Optional keyring password

Tick **Save password in the system keyring** (under the Password field) before
pressing **Save profile** to store the password too. This is strictly opt-in and
per profile. When it is ticked:

- an explicit **Profile name is mandatory** — a blank name is rejected with
  `Profile name is required when saving the password`;
- the password must be non-empty;
- the secret is written to the OS keyring under service `rustterm` and account
  = profile name (on Linux: GNOME Keyring / KWallet through Secret Service), so
  it is **never** written to `sessions.toml`;
- if the keyring is locked or unavailable, saving fails with a red error and the
  profile file is left untouched.

Loading such a profile fills the password box from the keyring. If the entry is
missing or cannot be read, an error is shown and you can type the password
manually; **Connect** always uses whatever is in the password box.

Unticking the box and saving again deletes the stored secret. Deleting the
profile also deletes its keyring entry. Entries appear in **Seahorse** (Passwords
and Keys) under `rustterm` / `<profile name>`, or with
`secret-tool search service rustterm`.

### Lean builds without keyring support

The keyring stack is optional at **compile time** (`keyring-secrets`, on by
default). Packagers or minimal installs can build without it:

```bash
cargo build --release --no-default-features
```

That drops 47 crates (the zbus/Secret Service stack) and about 2.3 MB from the
stripped binary. In such a build:

- the **Save password in the system keyring** checkbox is not shown;
- passwords are always typed manually and never stored by the app;
- a profile saved elsewhere with `save_password = true` still lists and loads,
  but shows *“this build has no keyring support”* instead of filling the
  password; re-saving it keeps the flag, so the secret is not orphaned if the
  full build is used again.

There is no runtime plugin download: Rust links the keyring code at build time.
Offering the feature later requires installing a build that includes it (or a
future separate helper binary).

Example file (the password itself never appears):

```toml
# rustterm saved sessions.
# Passwords are never stored here.

[[sessions]]
name = "legacy box"
host = "172.31.251.23"
port = 22
user = "anuragb"
save_password = true          # secret lives in the OS keyring
```

**Verify**

- [ ] Launch with no config file. The list says "No saved sessions yet."
- [ ] Fill Host `172.31.251.23`, Port `22`, User `anuragb`, leave the name
      blank, press **Save profile** → green `Saved profile 'anuragb@172.31.251.23'`
      and one entry appears without restarting.
- [ ] `cat ~/.config/rustterm/sessions.toml` → it contains name/host/port/user
      and **no plaintext password** (only an optional `save_password` flag).
- [ ] Click the saved entry → Host/Port/User are filled in, the password box is
      empty, the name box shows the profile name, and the entry is highlighted.
- [ ] Type the password and **Connect** → the session opens normally.
- [ ] **Name is mandatory for keyring saves**: tick **Save password in the
      system keyring**, clear the Profile name, press **Save profile** → red
      `Profile name is required when saving the password`; no file change.
- [ ] **Store**: enter name `legacy`, tick the box, type the password, press
      **Save profile** → green confirmation. The TOML gains
      `save_password = true` but no password text.
- [ ] **Load**: click another profile, then `legacy` → the password box is
      filled automatically from the keyring and the box stays ticked.
- [ ] Connect with the auto-filled password → login succeeds.
- [ ] The secret exists in the OS keyring: Seahorse shows `rustterm` / `legacy`,
      or `secret-tool search service rustterm` finds it.
- [ ] **Stop storing**: untick the box and press **Save profile** → green
      confirmation; the keyring entry is gone and loading `legacy` no longer
      fills a password.
- [ ] Re-tick, save, then **Delete selected** → profile and keyring entry both
      disappear.
- [ ] **Missing secret recovery**: remove the entry manually in Seahorse, click
      the profile → red `No password is stored for '…'`; type the password and
      **Connect** still works.
- [ ] **No keyring available** (only on a system without Secret Service): tick
      the box, press Save → red keyring error; no profile is written, and
      password-based connections still work.
- [ ] **Lean build**: `cargo run --no-default-features` → the keyring checkbox is
      absent; saving profiles and connecting with a typed password work as before.
- [ ] **Lean build + stored profile**: with a `save_password = true` profile from
      a full build, load it in the lean build → red “this build has no keyring
      support”; type the password and **Connect** still works. Re-saving keeps
      `save_password = true` in the TOML.
- [ ] Quit and `cargo run` again → the profile is still listed, and keyring
      passwords load again.
- [ ] Save a second profile with a custom name, e.g. `prod`. Both entries appear;
      the list scrolls after roughly five entries.
- [ ] With `prod` loaded, change the port and press **Save profile** again →
      still one `prod` entry, now with the new port (no duplicate).
- [ ] Select `prod`, press **Delete selected** → it disappears immediately and
      is gone after a restart.
- [ ] Clear Host, press **Save profile** → red `Host is required`; the file is
      unchanged.
- [ ] Set Port to `abc`, press **Save profile** → red `Port must be a number`.
- [ ] Set Port to `0`, press **Save profile** → red `Port must be between 1 and 65535`.
- [ ] Malformed-file resilience: back up the file, replace it with `garbage = =`,
      relaunch → a red parse error is shown but the app still starts and can
      connect manually. Saving a new profile replaces the malformed file.
- [ ] `XDG_CONFIG_HOME=/tmp/rt-config cargo run` → profiles are read/written
      under `/tmp/rt-config/rustterm/sessions.toml`.

---

## 13. Public-key and ssh-agent authentication — [U]

**How:** on the **New Session** tab there are two extra rows:

- **Public key** — tick **Use**. Leave the path blank to try the usual
  `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa` and `id_dsa` (only files that exist),
  or type an explicit private-key path. `~` and `~/...` are expanded.
- **SSH agent** — tick **Use identities from SSH_AUTH_SOCK** to offer every key
  currently loaded in ssh-agent.

Authentication is tried in this order: **key file(s) → ssh-agent → password**.
The password box is optional when key/agent auth is enabled, but if it is
filled in it remains a fallback after a rejected key. A passphrase-protected
key file is **not** decrypted by the app: add it to ssh-agent (`ssh-add`) and
use the agent option instead. If every method fails, the red error lists what
was tried (missing file, encrypted key, key rejected, agent unavailable, …).

These settings are saved with the profile (`use_key`, `key_path`, `use_agent`);
no key material or passphrase is ever written to `sessions.toml`. Old profiles
load with both options off.

**Verify**

Setup: a local sshd with `PubkeyAuthentication yes`, plus
`ssh-keygen -t ed25519 -f /tmp/rt-test-key -N ''` and its `.pub` appended to
`~/.ssh/authorized_keys`.

- [ ] **Explicit key**: Host/User set, type `/tmp/rt-test-key` in **Public key**,
      tick **Use**, leave the password empty, **Connect** → login succeeds.
- [ ] **Auto-detect**: clear the key path, keep **Use** ticked, connect with a
      normal `~/.ssh/id_*` key available → login succeeds.
- [ ] **`~` expansion**: use `~/.ssh/id_ed25519` as the path → login succeeds.
- [ ] **Agent**: `eval $(ssh-agent)` + `ssh-add ~/.ssh/id_ed25519` in the shell
      that launches `cargo run`, tick only the agent row, leave the password
      empty → login succeeds.
- [ ] **Password fallback**: key/agent enabled with a key the server rejects,
      correct password typed → login still succeeds (no error).
- [ ] **Encrypted key**: `ssh-keygen -t ed25519 -N 'x' -f /tmp/enc-key`, use it
      with no password → red error mentioning *passphrase-protected* and
      `ssh-add`; no crash.
- [ ] **Missing path**: `/tmp/does-not-exist`, no password → red error naming
      that path. Add the correct password → login still works.
- [ ] **Agent not running**: `env -u SSH_AUTH_SOCK cargo run`, agent row ticked,
      no key/password → red *could not connect to $SSH_AUTH_SOCK*.
- [ ] **No method**: all key/agent boxes off, password empty, **Connect** → red
      `Type a password or enable key/agent authentication` (no connection).
- [ ] **Profile round-trip**: save with a key path and agent ticked, load
      another profile, then reload it → path and both checkboxes restored.
      `cat ~/.config/rustterm/sessions.toml` shows `use_key`, `key_path`,
      `use_agent` and still **no key contents or passphrase**.
- [ ] **Legacy profile**: a profile file without the new fields loads with both
      options off and connects with a password.

---

## 14. SFTP side panel — [U]

**How:** after a successful connection, a **Remote files** panel appears on the
left. It opens at the remote login directory (`canonicalize(".")`, usually `$HOME`).
The **Files** toggle in the tab bar (next to the session tabs) shows/hides it, and
the panel's **×** hides it too. Each tab keeps its own directory.

- Entries are sorted **directories first**, then case-insensitive name. `📁` is a
  directory, `📄` a regular file, `🔗` a symlink (displayed but not followed yet).
- Click a **directory** to enter it. Click a **file** to select it; the footer
  shows its full path, size and age. Hovering any row shows the same details.
- **Home** returns to the login directory, **Up** goes to the parent (stops at
  `/`), **Refresh** re-lists the current directory.
- Type a remote path in the **Path** box and press **Enter** (or click **Go**).
  Relative paths are resolved by the server against the login directory.
- If the SSH server has no SFTP subsystem, the panel says **SFTP is unavailable**
  and shows the reason. The terminal is unaffected.
- **Limitation:** the panel does **not** follow `cd` in the shell yet (handoff
  F26). Use **Refresh**/the path box to change directory. Transfers are not
  implemented yet.

**Verify**

Setup: any server with `Subsystem sftp ...` enabled (the normal OpenSSH default),
and a shell you can type into.

- [ ] **Appears**: connect → the left panel lists the login directory; compare
      with `pwd` / `ls -la` in the terminal.
- [ ] **Sorting**: directories come first; names are case-insensitive
      alphabetical within each group.
- [ ] **Navigate**: click a directory → path and listing update. **Up** returns to
      the parent. **Home** returns to the login directory.
- [ ] **Path box**: type `/tmp` + **Enter** → `/tmp` is listed; repeat with **Go**.
      Type a bogus path → red error, previous listing stays usable.
- [ ] **Refresh**: `touch /tmp/rt-new-file` in the terminal, press **Refresh** in
      `/tmp` → the new file appears.
- [ ] **Select**: click a file → footer shows its path, human-readable size and
      relative age; hover a row shows the same details.
- [ ] **Toggle**: **Files** hides the panel; click again to show it. The panel
      **×** also hides it. Switching tabs shows the other tab's directory, and
      switching back restores the first tab's directory.
- [ ] **Background tab**: navigate tab A to `/tmp`, switch to tab B and run some
      output, switch back → tab A is still in `/tmp` and its listing is intact.
- [ ] **Responsiveness**: navigate to a very large directory (e.g. `/usr/bin`) and
      keep typing in the terminal → keystrokes echo while the listing loads; the
      terminal never freezes.
- [ ] **Unavailable**: connect to a server without the SFTP subsystem (or one that
      refuses a second channel) → **SFTP is unavailable** with a reason; the
      terminal still works and `cargo run` does not panic.
- [ ] **Symlink**: create `ln -s /etc /tmp/rt-link` and list `/tmp` → the link is
      shown with `🔗`; clicking it selects it (it does not navigate).
- [ ] **No auto-follow (expected)**: `cd /var/log` in the terminal → the panel
      stays where it was until you navigate/refresh manually.
- [ ] **Teardown**: close the tab with the panel open, and disconnect via
      **Disconnect** → no crash, no stuck thread.
