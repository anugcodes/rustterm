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

**How:** `+ New Session` is tab 0 and never closes. Fill Host/Port/User/Password
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

## 12. Saved profiles — [U]

**How:** on the **New Session** tab, fill in Host/Port/User, tick **Save as
profile**, optionally type a **Profile name** (leave it blank to use
`user@host`), and press **Connect**. The profile is written *before* the
connection starts, so a bad port, missing host or name error stops the attempt
with an inline message instead of connecting silently. Profiles are listed at the
bottom of the page in a compact **Name / Target** table: **Load** fills the form
back in and the row's **×** deletes the profile.

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
ticking **Save as profile** and connecting, to store the password too. This is
strictly opt-in and per profile. When it is ticked:

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

Unticking the box and connecting with **Save as profile** ticked again deletes
the stored secret. Deleting the profile also deletes its keyring entry. Entries
appear in **Seahorse** (Passwords and Keys) under `rustterm` / `<profile name>`,
or with `secret-tool search service rustterm`.

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

- [ ] Launch with no config file. The table says “Nothing saved yet. Tick
      “Save as profile” above to keep this connection.”
- [ ] Fill Host `172.31.251.23`, Port `22`, User `anuragb`, tick **Save as
      profile**, leave the name blank and press **Connect** → the profile is
      created before the connection attempt; `cat ~/.config/rustterm/sessions.toml`
      shows name/host/port/user and **no plaintext password**. Stop at the host-key
      prompt and close the tab if you do not want to log in.
- [ ] Press **Load** on an entry → Host/Port/User are filled in, the name field
      shows the profile name, and the password box is empty (unless a keyring
      password is stored).
- [ ] Type the password and **Connect** → the session opens normally.
- [ ] **Name is mandatory for keyring saves**: tick **Save password in the
      system keyring** and **Save as profile**, clear the name, press **Connect**
      → red `Profile name is required when saving the password`; no file change.
- [ ] **Store**: enter name `legacy`, tick both boxes, type the password, press
      **Connect** → saved and connected. The TOML gains `save_password = true`
      but no password text.
- [ ] **Load**: press **Load** on another profile, then `legacy` → the password
      box is filled automatically from the keyring and the box stays ticked.
- [ ] Connect with the auto-filled password → login succeeds.
- [ ] The secret exists in the OS keyring: Seahorse shows `rustterm` / `legacy`,
      or `secret-tool search service rustterm` finds it.
- [ ] **Stop storing**: untick the password box, tick **Save as profile**, press
      **Connect** → the keyring entry is gone and loading `legacy` no longer
      fills a password.
- [ ] Re-tick, save, then press the row's **×** → the profile and keyring entry
      both disappear.
- [ ] **Missing secret recovery**: remove the entry manually in Seahorse, press
      **Load** on the profile → red `No password is stored for '…'`; type the
      password and **Connect** still works.
- [ ] **No keyring available** (only on a system without Secret Service): tick
      the box, press Connect → red keyring error; no profile is written, and
      password-based connections still work.
- [ ] **Lean build**: `cargo run --no-default-features` → the keyring checkbox is
      absent; saving profiles and connecting with a typed password work as before.
- [ ] **Lean build + stored profile**: with a `save_password = true` profile from
      a full build, load it in the lean build → red “this build has no keyring
      support”; type the password and **Connect** still works. Re-saving keeps
      `save_password = true` in the TOML.
- [ ] Quit and `cargo run` again → the profiles are still listed, and keyring
      passwords load again.
- [ ] Save a second profile with a custom name, e.g. `prod`. Both entries appear;
      the list scrolls after four rows.
- [ ] Load `prod`, change the port, tick **Save as profile** and Connect →
      still one `prod` entry, now with the new port (no duplicate).
- [ ] Press the row's **×** on `prod` → it disappears immediately and is gone
      after a restart.
- [ ] Clear Host, tick **Save as profile**, press **Connect** → red `Host is
      required`; the file is unchanged and no connection is attempted.
- [ ] Set Port to `abc`, press **Connect** → red `Port must be a number`.
- [ ] Set Port to `0`, press **Connect** → red `Port must be between 1 and 65535`.
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

**How:** after a successful connection, the remote file panel appears on the
left. It opens at 450 px, can be dragged between 340 px and 55% of the window
(capped at 720 px), so the terminal always stays visible. It starts at the remote
login directory (`canonicalize(".")`, usually `$HOME`) and lists entries in a
table with a fixed header: **Name**, **Owner**, **Modified** and **Size**. The
**Files** toggle in the tab bar shows/hides it, and the panel's **×** hides it
too. Each tab keeps its own width, directory and selection.

- Entries are sorted **directories first**, then case-insensitive name. The
  font-safe icons are `🗀` for a directory, `🗋` for a regular file and `🔗` for a
  symlink (displayed but not followed yet).
- Rows are striped and stay on one line; long names/owners/times are elided.
  The header uses the same font size as the rows and nothing is bold.
- Column dividers exist only in the header. Drag one to resize the column to its
  right; file rows never show resize handles.
- Hover a row for its full path, type, size, owner, exact timestamp and relative
  age.
- **Single click** selects a row. **Double click** opens it: folders navigate,
  files open the **Open with…** picker. A single click never opens a menu.
- **Right-click** selects the row and opens its menu. File: **Open with…**,
  **Rename…**, **Copy path**, **Delete…**. Folder: **Open**, **Paste into this
  folder** (when something has been copied), **Rename…**, **Copy path**,
  **Delete…**.
- The icon toolbar (hover for a tooltip) provides 🏠 **Home** (login directory),
  ⬆ **Up**, 🔄 **Refresh**, ➕ **New folder**, 🗐 **Copy**, 📋 **Paste**,
  👁 **Hidden** (dotfiles, per tab), ⚡ **Reconnect** and × **Hide**.
- **Status bar:** the bottom of the panel always shows the latest message with a
  colour-coded icon. While a listing or file operation runs it shows a spinner
  and the operation label instead. The ☰ button opens the last 50 messages with
  timestamps, plus **Clear messages**. There is no selected-file footer; row
  tooltips carry the details.
- Type a remote path in the **Path** box and press **Enter** (or click ➡).
  Relative paths are resolved by the server against the login directory.
- **Hidden** shows or hides dotfiles (default hidden). The setting is per tab and
  survives refresh/navigation. If the selected row becomes hidden, the selection
  is cleared.
- **New folder** creates a folder in the current directory. **Copy/Paste**:
  **Copy** remembers a remote file or folder (no data moves yet), **Paste**
  duplicates it into the current directory as `name (copy)`, `name (copy 2)`, …
  Files stream through the client; directories recurse; symlinks are recreated
  with their original target. A folder cannot be pasted into itself.
- **Open with…** opens a modal picker: **Default editor** (`$VISUAL`, then
  `$EDITOR`), **Zed**, **gvim**, **VS Code**, **Desktop default** (`xdg-open`),
  or a **Custom command** such as `kate --new`. Editors that are not on `PATH`
  are shown disabled. The last choice is preselected for the session. The file
  is downloaded to a unique `rustterm-edit-<pid>-<nanos>` temp directory and
  launched asynchronously.
- **Rename/Delete** use the same compact, auto-sized modal with right-aligned
  Cancel plus the primary action. Delete names the entry, shows the remote path
  and warns that the action cannot be undone (tinted red, and worded for
  recursive folder removal). `Esc` cancels. Rename accepts a single path
  component (no `/`).
- If the SSH server has no SFTP subsystem, the panel says **SFTP is unavailable**
  with the reason in the status bar and offers **Reconnect SFTP**. Only the SFTP
  channel is retried; the terminal is unaffected.
- **Limitations:** the panel does **not** follow `cd` in the shell yet (handoff
  F26). Use **Refresh**/the path box to change directory. General upload/
  download, drag & drop and progress bars are still on the transfers step.
  Symlinks are shown/copied but double-clicking one does not navigate. Editor
  changes are not watched or uploaded.

**Verify**

Setup: any server with `Subsystem sftp ...` enabled (the normal OpenSSH default),
and a shell you can type into. Use a scratch directory such as `/tmp/rt-sftp`
so delete/copy tests cannot touch real data.

- [ ] **Appears**: connect → the left panel lists the login directory; compare
      with `pwd` / `ls -la` in the terminal.
- [ ] **Columns**: the header reads **Name / Owner / Modified / Size**, uses the
      same font size as the rows, is not bold and stays visible while scrolling.
      Owner and local `YYYY-MM-DD HH:MM` match `stat -c '%U %y' file`; folders
      show an em dash for size. Drag a divider in the header to resize the column
      to its right (dividers are not drawn on file rows) and narrow the panel to
      ~340 px: text elides instead of wrapping, and the header remains readable.
- [ ] **Sorting**: directories come first; names are case-insensitive
      alphabetical within each group.
- [ ] **Select vs open**: one click highlights a row without navigating; double
      click a folder → path and listing update; double click a file → the
      **Open with…** modal appears.
- [ ] **Right-click menu**: right-click a file → Open with…, Rename…, Copy path,
      Delete…; right-click a folder → Open, Rename…, Copy path, Delete…, plus
      Paste into this folder after copying an entry. A left click does not open
      this menu. **Copy**, **New folder…** and **Refresh** are absent by design.
- [ ] **Toolbar**: every icon shows a tooltip; Home/Up/Refresh/New folder/Copy/
      Paste/Hidden/Reconnect/Hide all work. Up, Copy and Paste are disabled when
      they cannot run.
- [ ] **Path box**: type `/tmp` + **Enter** → `/tmp` is listed; repeat with ➡.
      Type a bogus path → an error in the status bar, previous listing stays.
- [ ] **Refresh**: `touch /tmp/rt-new-file` in the terminal, press 🔄 in `/tmp`
      → the new file appears.
- [ ] **Status bar**: the panel bottom shows the latest message; it is not
      clipped and does not overlap the table. Start a large folder copy → a
      spinner and the operation label appear. After it finishes the success
      message remains visible (not overwritten by the automatic refresh). Click
      ☰ → timestamps and the message list; **Clear messages** empties it.
- [ ] **Hidden files**: `touch /tmp/.rt-hidden` → by default it is absent; toggle
      👁 → it appears; toggle again → it disappears. Navigate away/back; the
      state is preserved per tab.
- [ ] **New folder**: use ➕ → enter a name → folder appears after the automatic
      refresh. Try a name containing `/`, empty, `.` and `..` → the dialog keeps
      an inline error.
- [ ] **Rename**: right-click a file → **Rename…** → new name appears; the old
      name is gone. Renaming to the same name is rejected. `Esc` cancels.
- [ ] **Copy/paste file**: right-click a file → **Copy** (toolbar) → paste in the
      same folder → `file (copy)` appears with the same contents (`cmp`). Paste
      again → `file (copy 2)`.
- [ ] **Copy/paste folder**: create `/tmp/rt-sftp/a` with files/subfolders and a
      symlink, copy it, paste into `/tmp/rt-sftp/b` → tree and contents match;
      the symlink still points at the original target (not a duplicate file).
- [ ] **Paste guard**: copy `/tmp/rt-sftp/a`, right-click it → **Paste into this
      folder** → error in the status bar; no runaway copy.
- [ ] **Copy path**: right-click a row → **Copy path** → paste into a local text
      field/terminal → the remote path appears (uses the system clipboard).
- [ ] **Delete**: right-click a file → **Delete…** → the dialog is centred and
      padded, names the file and path, and Cancel/Delete are right-aligned.
      Confirm → file is gone. Create a non-empty folder, delete it → warning is
      tinted and says the whole tree is removed; cancel leaves everything
      intact. `Esc` cancels.
- [ ] **Open with…**: double-click a small text file → modal lists Default
      editor, Zed, gvim, VS Code, Desktop default and Custom command. Editors
      that are not installed are disabled. Choose one → an editor opens with the
      file contents and the status bar names the temp download path. Reopen the
      picker → the same choice is preselected. Choose **Custom command** with an
      empty field → inline error; with `kate --new` → Kate opens. Close the
      editor; the app remains responsive.
- [ ] **Reconnect**: click ⚡ → the SFTP listing reopens at the login directory;
      the terminal scrollback and running shell are untouched. Repeat while a
      command is streaming output in the terminal.
- [ ] **Unavailable / retry**: connect to a server without the SFTP subsystem (or
      one that refuses a second channel) → **SFTP is unavailable** with a reason;
      the status bar repeats it. The terminal still works. If the channel can be
      opened later, **Reconnect SFTP** recovers without reconnecting SSH.
- [ ] **Symlink**: create `ln -s /etc /tmp/rt-link` and list `/tmp` → the link is
      shown with `🔗`; clicking selects it, double-clicking does not navigate.
      Copy/paste it → the duplicate is still a symlink to `/etc`.
- [ ] **Toggle**: **Files** hides the panel; click again to show it. The panel
      **×** also hides it. Switching tabs shows the other tab's directory,
      clipboard, message history and hidden-file state.
- [ ] **Background tab**: navigate tab A to `/tmp`, switch to tab B and run some
      output, switch back → tab A is still in `/tmp` and its listing is intact.
- [ ] **Responsiveness**: navigate to a very large directory (e.g. `/usr/bin`),
      start a large folder copy, and keep typing in the terminal → keystrokes echo
      and output flows while the listing/copy runs.
- [ ] **No auto-follow (expected)**: `cd /var/log` in the terminal → the panel
      stays where it was until you navigate/refresh manually.
- [ ] **Teardown**: close the tab with the panel open, and disconnect via the
      **Disconnect** bar at the bottom of the terminal → no crash, no stuck
      thread.
- [ ] **Window cap**: with the panel open, shrink the window to ~900 px wide →
      the panel stops at 55% (about 495 px) and the terminal stays usable; drag
      the panel wider and it refuses to cover the terminal.

## 15. Appearance and themes — [V]

**How:** the **🎨 Theme** button at the far right of the tab bar opens a short
menu with the built-in themes. Selecting one applies it to the whole app
immediately — top bar, forms, cards, tables, dialogs and the SFTP panel — and
closes the menu. The design is deliberately flat and boxy: square corners, 1 px
borders, no drop shadows and no accent-coloured hover glow. All controls share
the same padding and a 26 px control height. The choice is saved in:

```
$XDG_CONFIG_HOME/rustterm/settings.toml      # if XDG_CONFIG_HOME is set
~/.config/rustterm/settings.toml             # otherwise
```

and restored at startup. The terminal's ANSI colours are intentionally
independent of the UI theme; only the application chrome changes.

Popups follow one geometry: every context/dropdown menu is 220 px wide with
22 px rows and no vertical gaps (a long file name is truncated in the menu
header, with the full path in a tooltip), and every modal — Rename, New folder,
Delete, host key and **Open with…** — is 440 px wide. The tokens live in
`src/theme.rs` (`MENU_WIDTH`, `MENU_ROW_HEIGHT`, `DIALOG_WIDTH`); new menus call
`theme::menu_style` and `theme::menu_item` so they inherit the same look.

Current themes: **Midnight** (default, cool dark blue) and **Graphite** (warm
neutral grey).

Adding a theme:

1. Add a variant to `ThemeId` in `src/theme.rs` and include it in `ThemeId::ALL`.
2. Map it in `ThemeId::{id,name,theme}` and add a `Theme`/`Palette` constant.
3. Reuse `Palette`/`Metrics`; screens read these tokens rather than hard-coded
   colours, so the new theme applies everywhere automatically.

**Verify**

- [ ] Theme button sits at the far-right edge of the tab bar and shows a hover
      tooltip, at small and large window widths, with several tabs open.
- [ ] Switching Midnight ↔ Graphite recolours the tab bar, New Session cards,
      tables, dialogs and status bar without restarting. No text becomes
      unreadable.
- [ ] Restart the app → the last theme is still selected.
- [ ] Terminal output colours are unchanged by the UI theme.

## 16. New Session page — [V]

The **New Session** tab is a centred column (maximum 600 px) inside a vertical
scroll area. One card holds the whole flow, top to bottom:

- **Connection**: Host + Port on one row, then User and Password. Labels share a
  single gutter so the inputs line up; text is vertically centred in every
  26 px input.
- **Use a private key** reveals a key-path field (blank auto-detects
  `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa` or `id_dsa`). **Use SSH agent
  identities** is next. The optional **Save password in the system keyring**
  checkbox is hidden in lean builds.
- **Save as profile**: ticking the checkbox enables the profile-name field
  (blank = `user@host`). Connect stores the profile first and aborts on an
  invalid port/name. The status line below the button reports save and connect
  results in place.
- A separator, then the **Saved profiles** table: a heading with the profile
  count (hover it to see the store path), **Name / Target** columns, **Load**
  and **×** per row, and an empty-state hint when nothing is saved.

The page keeps equal padding, uses theme tokens rather than hard-coded sizes,
and scrolls when the window is short.

**Verify**

- [ ] At ~900 px and ~1600 px window widths the column stays centred and does
      not stretch; no horizontal scrollbar appears.
- [ ] Shrink the window vertically → the page scrolls; no card or table row is
      clipped.
- [ ] The **Save as profile** name field is disabled until the checkbox is
      ticked, just like the private-key path field.
- [ ] Press **Connect** with empty fields → the status line inside the card
      explains what is missing and the button is usable again.
- [ ] At ~700 px window width the Host/Port row and saved-profile columns still
      fit; long profile targets elide with a tooltip.

## 17. File transfers — [U]

**How:** uploads and downloads run over the existing SFTP channel, so they work
whenever the panel is usable. There are four entry points:

- **⬇ toolbar button** — downloads the selected row into a folder you pick.
- **⬆ toolbar button** — uploads one or more local files into the current remote
  directory.
- **Row menu** — **Download…** on any file or folder; folders also offer
  **Upload files here…** and **Upload folder here…**.
- **Drag & drop** — drop files or folders from the desktop anywhere over the
  SFTP panel to upload them into the visible remote directory.

A transfer bar appears above the status bar while something is running. It shows
the label (`Downloading notes.txt…`), a byte progress bar with `done / total`,
`files done/total`, a **×** cancel button and `+N queued` when more work waits.
One transfer runs per session; further requests are queued in order.

**Behaviour**

- Files are written to a sibling `.part` file and renamed only when complete, so
  an interrupted transfer never leaves a truncated file under the real name.
  Cancelling (or a failure) removes the `.part`.
- Downloads recreate folders recursively and re-create symlinks with their
  original target; uploads do the same in reverse. Empty folders are preserved.
- When a top-level destination already exists the **Items already exist** modal
  lists up to five names and offers **Replace**, **Keep both** or **Cancel**
  (`Esc` cancels; the transfer has not started yet).
  - **Replace** overwrites files and merges into existing folders.
  - **Keep both** writes the incoming top-level item as `name (copy)`, with
    `name (copy 2)` and so on if that is taken too. Nested items follow the
    merge behaviour.
- Cancel stops the running transfer at the next 64 KiB chunk. If nothing is
  running it drops the queue instead.
- The listing refreshes when a transfer finishes, and the status bar reports the
  outcome (`Downloaded 3 file(s) to /home/you/Downloads`). Failures are collected
  and reported together rather than aborting the whole batch.
- The terminal stays responsive throughout: transfers run on the session thread
  and only progress atomics are read per frame.

**Verify**

- [ ] **Download a file**: ⬇ with a file selected → choose a folder → the bar
      fills, then the status bar names the destination and the local file exists
      with identical contents (`cmp`).
- [ ] **Download a folder**: right-click a non-empty folder → **Download…** →
      the whole tree, including sub-folders and symlinks, lands in the chosen
      folder.
- [ ] **Upload files**: ⬆ in `/tmp` → pick two files → both appear there, and the
      listing refreshes. Re-open the picker and cancel → nothing happens.
- [ ] **Upload folder**: right-click a remote folder → **Upload folder here…** →
      the local tree is created inside that folder, not the current directory.
- [ ] **Drag & drop**: drag a file from the file manager onto the panel → it is
      uploaded into the visible directory.
- [ ] **Conflict → Replace**: upload a file that already exists → modal names it
      → **Replace** overwrites it.
- [ ] **Conflict → Keep both**: repeat with **Keep both** → `name (copy)` appears
      and the original is untouched; repeating gives `name (copy 2)`.
- [ ] **Conflict → Cancel / Esc**: neither the original nor a copy changes and
      no transfer is queued.
- [ ] **Queue**: start a large download and immediately start another → the bar
      shows `+1 queued`, the second starts automatically when the first ends.
- [ ] **Cancel**: start a large transfer and press **×** → the bar says
      `cancelling…`, then `Transfer cancelled`, and no `.part` file remains.
- [ ] **Failure**: download into a read-only local folder → the status bar shows
      the error, the listing still works and the session stays connected.
- [ ] **Responsiveness**: during a large transfer, type in the terminal → input
      echoes normally.