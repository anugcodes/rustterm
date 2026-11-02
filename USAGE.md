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
- [ ] Start a connect to a blackholed host (e.g. `10.255.255.1`) → press **✕**
      next to *Connecting to…* → the form returns immediately with
      *Connection cancelled*.
- [ ] Wrong password → the form says *Login failed — the server rejected this
      username or password.*
- [ ] Wrong port (e.g. `2222`) → *Connection refused — nothing is listening on
      that host and port.*
- [ ] Unknown hostname → *Host not found — check the hostname.*
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
table with a fixed header: **Name**, **Modified**, **Size** and **Owner**. The
**Files** toggle in the tab bar shows/hides it, and the panel's **×** hides it
too. Each tab keeps its own width, directory, selection, column widths and sort.

The table is its own **fixed 430 px canvas**. The panel is only a window onto it:
shrinking the panel clips the table and adds a horizontal scrollbar; growing it
reveals more of the canvas (or blank space past Owner). Resizing the panel never
reflows the columns or hides one.

- Entries are **folders first**, then ordered by the active column. Click a
  header to sort by it; click it again to reverse. The active column shows a
  ⏶/⏷ arrow and an underline. Name is the default (ascending,
  case-insensitive).
- The font-safe icons are `🗀` for a directory, `🗋` for a regular file and `🔗`
  for a symlink. Double-clicking a symlink follows it: a folder target is
  listed through the link path (so `/tmp/rt-link` stays the breadcrumb) and a
  file target opens the **Open with…** picker.
- Rows are striped and stay on one line. Column widths are stored in pixels and
  never depend on the panel width. A long name elides inside its column; the
  full path is always available in the details strip.
- The header uses the same font size as the rows and nothing is bold.
- Column dividers exist only in the header. Drag one to resize the two columns
  around it; the divider snaps to **24 px steps**, so it moves one notch at a
  time instead of following the pointer freely. A column cannot be dragged
  below its minimum readable width. File rows never show resize handles.
- Selecting a row shows a **two-line details strip** above the status bar with
  the full remote path, type, exact size, owner, exact timestamp and relative
  age. It disappears when nothing is selected, giving the height back to the
  table.
- Hovering a row still shows a compact tooltip with its path, size, owner and
  timestamp.
- **Single click** selects a row. **Double click** opens it: folders navigate,
  symlinks are followed, files open the **Open with…** picker. A single click
  never opens a menu.
- **Right-click** selects the row and opens its menu. File: **Open with…**,
  **Rename…**, **Copy path**, **Delete…**. Folder: **Open**, **Paste into this
  folder** (when something has been copied), **Rename…**, **Copy path**,
  **Delete…**. A symlink also gets **Follow link** at the top of its menu.
- The icon toolbar (hover for a tooltip) provides 🏠 **Home** (login directory),
  📂 **Up one level**, 🔄 **Refresh**, ➕ **New folder**, 🗐 **Copy**,
  📋 **Paste**, 📥 **Download**, 📤 **Upload**, 👁 **Hidden** (dotfiles, per
  tab), ⚡ **Reconnect** and × **Hide**. Every icon is checked against the
  bundled fonts by `theme::tests::icons_have_glyphs`, so a missing glyph cannot
  come back as an empty box. The toolbar no longer draws a spinner next to
  **Reconnect**; the status bar reports the reconnect attempt.
- **Status bar:** the bottom of the panel always shows the latest message with a
  colour-coded icon. While a listing or file operation runs it shows a spinner
  and the operation label; a transfer's queue depth is appended there as
  `(+N queued)`. The ☰ button opens the last 50 messages with timestamps, plus
  **Clear messages**.
- Type a remote path in the **Path** box and press **Enter** (or click ➡).
  Relative paths are resolved by the server against the login directory.
- **‹ / ›** step through the directories you have visited (up to 100) when
  **Show Back/Forward history buttons** is enabled in **Settings → File panel**
  (off by default). History is committed only after a listing succeeds, so a
  typo or a permission error never leaves Back pointing at a folder that would
  not open. A new navigation after going back discards the forward stack.
- The **Follow** chip sits at the right of the path row. It is on by default:
  the panel follows the shell's working directory. Any manual move (path box,
  **Up**, **Home**, a folder click, **Back/Forward**) pauses following until
  **Follow** is clicked again. While the path box has focus, following waits so
  a prompt cannot overwrite what you are typing. Right-click the chip for
  **Set up directory tracking…**, which reopens the setup dialog after the hint
  has been dismissed.
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
  launched asynchronously. The **Upload changes automatically when the file is
  saved** checkbox (on by default, remembered per session) turns the download
  into a two-way edit — see §18.
- **Rename/Delete** use the same compact, auto-sized modal with right-aligned
  Cancel plus the primary action. The heading names the entry
  (`Rename “old.name”`, `Delete “old.name”`); delete shows the remote path
  (wrapped, not truncated) and warns that the action cannot be undone (tinted
  red, and worded for recursive folder removal). `Esc` cancels. Rename accepts
  a single path component (no `/`).
- If the SSH server has no SFTP subsystem, the panel says **SFTP is unavailable**
  with the reason in the status bar and offers **Reconnect SFTP**. Only the SFTP
  channel is retried; the terminal is unaffected.
- **Limitations:** following the shell needs a cooperating shell (see below).
  Without it the panel still works; use **Refresh** or the path box to move.
  Symlinks are followed on demand rather than automatically during a listing.

### Following the shell (OSC 7)

A shell announces its directory with an OSC 7 escape sequence
(`ESC ] 7 ; file://host/path BEL`). The terminal view scans the byte stream for
it (split chunks included, percent-decoded), so the panel can move whenever you
`cd`, including inside `tmux` or over a slow link.

Most stock shells do not send it. About three seconds after the first listing,
if no directory has been reported, a small strip under the path row offers three
choices:

- **Enable now** types a one-line snippet into the current shell. The login
  shell is detected first from `/etc/passwd`, then the matching hook is used:
  bash `PROMPT_COMMAND`, zsh `precmd_functions`, or the csh/tcsh `cwdcmd`
  alias. Nothing is written to disk. It takes effect at the next prompt.
  Pressing it twice is harmless. It is disabled while a full-screen program
  (vim, htop) is running.
- **Install…** detects your login shell from `/etc/passwd`, then asks before
  editing the startup file that shell actually reads: `~/.bashrc`, `~/.zshrc`,
  `~/.cshrc`, or `~/.tcshrc` when that file already exists (tcsh reads it
  instead of `~/.cshrc`). The block is wrapped in
  `# >>> rustterm directory tracking >>>` / `# <<< ... <<<` markers. The first
  install writes a one-time `<file>.rustterm-backup`; installing again replaces
  the block instead of appending a second one. The dialog's **Remove** deletes
  the marked block and leaves the rest of the file byte-for-byte intact.
  Fish and other shells are not supported and report that in the status bar.
- **×** dismisses the hint for this session.

csh and tcsh have a single `cwdcmd` alias, so **Install** replaces one you
already have. The dialog warns about this, keeps the original in the backup, and
**Copy snippet** copies the one-liner so you can add it yourself instead. The
snippet is syntax-checked with `bash -n` (and by a real `csh`/`tcsh` when one is
installed) in the test suite. Nothing is ever written without clicking
**Install…** and confirming in the dialog.

**Verify**

Setup: any server with `Subsystem sftp ...` enabled (the normal OpenSSH default),
and a shell you can type into. Use a scratch directory such as `/tmp/rt-sftp`
so delete/copy tests cannot touch real data.

- [ ] **Appears**: connect → the left panel lists the login directory; compare
      with `pwd` / `ls -la` in the terminal.
- [ ] **Columns**: the header reads **Name / Modified / Size / Owner**, uses the
      same font size as the rows, is not bold and stays visible while scrolling.
      Name is about half the table. Owner and local `YYYY-MM-DD HH:MM` match
      `stat -c '%U %y' file`; folders show an em dash for size. Drag a divider
      in the header: it moves in **24 px** steps and only the two neighbouring
      columns change (dividers are not drawn on file rows). Narrow the panel to
      ~340 px: the table keeps its column widths, is clipped at the panel edge
      and gains a horizontal scrollbar; widen the panel and the same table is
      revealed again.
- [ ] **Sorting**: directories come first, then the active column. Click
      **Size** / **Modified** / **Owner** in the header → the arrow (⏶/⏷)
      appears, the order changes, and a second click reverses it. Click
      **Name** to restore the default case-insensitive name order.
- [ ] **Details strip**: select a row → two lines above the status bar show the
      full path, type, size, owner, exact timestamp and age. Click a different
      row → it updates; refresh/navigate → it disappears with the selection.
- [ ] **Select vs open**: one click highlights a row without navigating; double
      click a folder → path and listing update; double click a file → the
      **Open with…** modal appears.
- [ ] **Right-click menu**: right-click a file → Open with…, Rename…, Copy path,
      Delete…; right-click a folder → Open, Rename…, Copy path, Delete…, plus
      Paste into this folder after copying an entry. A left click does not open
      this menu. **Copy**, **New folder…** and **Refresh** are absent by design.
- [ ] **Toolbar**: every icon draws a real glyph (no empty boxes) and shows a
      tooltip; 🏠 Home / 📂 Up / 🔄 Refresh / ➕ New folder / 🗐 Copy / 📋 Paste /
      📥 Download / 📤 Upload / 👁 Hidden / ⚡ Reconnect / × Hide all work. Up,
      Copy and Paste are disabled when they cannot run.
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
- [ ] **Rename**: right-click a file → **Rename…** → the heading reads
      `Rename “old.name”`; type a new name → it appears and the old name is
      gone. Renaming to the same name is rejected. `Esc` cancels.
- [ ] **Popup layout**: every modal heading is the smaller 15 px style; a very
      long entry name truncates with a tooltip while the path/destination below
      wraps onto multiple lines instead of being cut off.
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
      editor, Zed, gvim, VS Code, Desktop default and Custom command, plus the
      ticked **Upload changes automatically when the file is saved** checkbox.
      Editors that are not installed are disabled. Choose one → an editor opens
      with the file contents and the status bar says the file will be uploaded
      on save. Reopen the picker → the same choice is preselected. Choose
      **Custom command** with an empty field → inline error; with `kate --new` →
      Kate opens. Close the editor; the app remains responsive. Save-to-upload
      behaviour is covered in §18.
- [ ] **Reconnect**: click ⚡ → the SFTP listing reopens at the login directory;
      the terminal scrollback and running shell are untouched. Repeat while a
      command is streaming output in the terminal.
- [ ] **Unavailable / retry**: connect to a server without the SFTP subsystem (or
      one that refuses a second channel) → **SFTP is unavailable** with a reason;
      the status bar repeats it. The terminal still works. If the channel can be
      opened later, **Reconnect SFTP** recovers without reconnecting SSH.
- [ ] **Symlink**: create `ln -s /etc /tmp/rt-link` and
      `ln -s /etc/hostname /tmp/rt-hn`, then list `/tmp` → both show `🔗`.
      Double-click the folder link → `/tmp/rt-link` is listed and the path box
      keeps the link name; double-click the file link → the **Open with…**
      picker. Right-click → **Follow link** behaves the same. Copy/paste a link
      → the duplicate is still a symlink to the original target. A dangling link
      reports `broken symlink …` in the status bar.
- [ ] **History**: visit `/tmp`, `/etc`, `/usr` → ‹ is enabled and › is
      disabled. ‹ twice → `/tmp`; › → `/etc`; then navigate to `/var` → › is
      disabled again. A failed path (e.g. `/no-such-dir`) must not add a history
      entry.
- [ ] **Follow (default)**: with the shell hook active, `cd /var/log` → the
      panel follows the terminal. `cd` into a directory you cannot list → one
      error, then the panel does not retry it on every prompt.
- [ ] **Manual pause**: click **Up**, type a path, or use ‹/› → the **Follow**
      chip turns off and later `cd`s do not move the panel. Click **Follow** →
      it jumps to the shell's current directory (not its directory at the time
      following stopped).
- [ ] **Typing guard**: focus the path box, run `cd /etc` in the terminal → the
      panel stays put. Click elsewhere (blur) → the next directory report moves
      it again.
- [ ] **Hook hint**: in a session with no hook, wait ~3 s at the first listing →
      the strip appears under the path row. **×** hides it for the session.
      **Enable now** types the snippet; press **Enter** if needed, then `cd` →
      the panel follows and the strip disappears. After dismissing with **×**,
      right-click the **Follow** chip → **Set up directory tracking…** opens the
      same dialog.
- [ ] **Install / Remove**: **Install…** → the dialog names the detected shell
      and the startup file it reads (`~/.bashrc`, `~/.zshrc`, `~/.cshrc`, or an
      existing `~/.tcshrc`). **Install** → the block is appended, a
      `.rustterm-backup` is created and the status bar confirms it. Install
      again → no duplicate block. **Remove** → the block is gone and the rest of
      the file is unchanged. A new login still has tracking (open a new tab).
- [ ] **csh / tcsh**: as an account whose login shell is `/bin/csh` or
      `/bin/tcsh`, **Enable now** types the `cwdcmd` snippet and `cd` in the
      terminal moves the panel. **Install…** warns that the single `cwdcmd`
      alias will be replaced; **Copy snippet** copies the same one-liner.
- [ ] **Unsupported shell**: connect as an account whose login shell is `fish`
      (or `/bin/sh`) → **Install…** reports that bash, zsh, csh and tcsh are
      supported; no file is written.
- [ ] **Toggle**: **Files** hides the panel; click again to show it. Switching
      tabs shows the other tab's directory, clipboard, message history and
      hidden-file state.
- [ ] **Background tab**: navigate tab A to `/tmp`, switch to tab B and run some
      output, switch back → tab A is still in `/tmp` and its listing is intact.
- [ ] **Responsiveness**: navigate to a very large directory (e.g. `/usr/bin`),
      start a large folder copy, and keep typing in the terminal → keystrokes echo
      and output flows while the listing/copy runs.
- [ ] **Teardown**: close the tab with the panel open → no crash, no stuck
      thread.
- [ ] **Window cap**: with the panel open, shrink the window to ~900 px wide →
      the panel stops at 55% (about 495 px) and the terminal stays usable; drag
      the panel wider and it refuses to cover the terminal.

## 15. Appearance and themes — [V]

**How:** the **Settings** button at the far right of the tab bar opens a
dedicated window (not a tab). **Appearance** lists the built-in themes;
selecting one applies it to the whole app immediately — top bar, forms, cards,
tables, dialogs and the SFTP panel. The design is deliberately flat and boxy:
square corners, 1 px borders, no drop shadows and no accent-coloured hover glow.
All controls share the same padding and a 26 px control height. The same window
holds **File panel** (Back/Forward history buttons) and **Remote display**
(§19). The choice is saved in:

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

- [ ] **Settings** sits at the far-right edge of the tab bar and shows a hover
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

- **📥 toolbar button** — downloads the selected row into a folder you pick.
- **📤 toolbar button** — uploads one or more local files into the current remote
  directory. (**📂** is the separate "up one level" button.)
- **Row menu** — **Download…** on any file or folder; folders also offer
  **Upload files here…** and **Upload folder here…**.
- **Drag & drop** — drop files or folders from the desktop anywhere over the
  SFTP panel to upload them into the visible remote directory.

Progress is shown in a compact transfer row above the status bar: a byte bar
with `done / total`, `files done/total` and a **×** cancel button. The status bar
above it names the transfer (`Downloading notes.txt…`) and appends the queue
depth as `(+1 queued)`, so the text truncates safely and cannot overlap the
progress widgets. One transfer runs per session; further requests are queued in
order.

**Behaviour**

- Files are written to a sibling `.part` file and renamed only when complete, so
  an interrupted transfer never leaves a truncated file under the real name.
  Cancelling (or a failure) removes the `.part`.
- Downloads recreate folders recursively and re-create symlinks with their
  original target; uploads do the same in reverse. Empty folders are preserved.
- When a top-level destination already exists the **Replace “name”** modal
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
  and only progress atomics are read per frame. Walking a large local folder for
  upload happens on a blocking worker, so terminal I/O keeps flowing.
- The client advertises **no SSH compression**. `russh 0.45`'s outbound zlib
  compressor truncates incompressible packets (PDFs, images, archives), which
  made the server drop the whole connection mid-upload. Compression stays off
  until a russh upgrade is done.
- If the transport dies during a transfer, the queue is dropped and the status
  bar says the connection closed instead of repeating a raw library error.
  The tab then shows a **Connection lost** banner with **Close tab**; scrollback
  is preserved. Keepalives (15 s, three misses) make this appear within about
  45 s instead of leaving the tab frozen forever.
- SFTP requests time out after 30 s (network mounts can be slow to answer a cold
  `stat`). A timeout marks the panel unavailable with a **Reconnect SFTP** hint
  rather than a per-file error; the terminal stays connected.

**Verify**

- [ ] **Download a file**: 📥 with a file selected → choose a folder → the bar
      fills, then the status bar names the destination and the local file exists
      with identical contents (`cmp`).
- [ ] **Download a folder**: right-click a non-empty folder → **Download…** →
      the whole tree, including sub-folders and symlinks, lands in the chosen
      folder.
- [ ] **Upload files**: 📤 in `/tmp` → pick two files → both appear there, and the
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
- [ ] **Queue**: start a large download and immediately start another → the
      status bar shows `(+1 queued)`, the second starts automatically when the
      first ends.
- [ ] **Upload a PDF**: upload a several-hundred-page PDF (incompressible) → the
      bar advances, the remote size matches `ls -l`, and the session does not
      disconnect. `cmp` the two copies.
- [ ] **Upload a folder**: upload a tree mixing text, PDFs and images (~100 MB)
      → every file arrives, the terminal still echoes input during the walk, and
      the session stays connected afterwards.
- [ ] **Cancel**: start a large transfer and press **×** → the bar says
      `cancelling…`, then `Transfer cancelled`, and no `.part` file remains.
- [ ] **Failure**: download into a read-only local folder → the status bar shows
      the error, the listing still works and the session stays connected.
- [ ] **Responsiveness**: during a large transfer, type in the terminal → input
      echoes normally.
- [ ] **Connection loss**: kill the sshd side (`sudo systemctl stop ssh` or
      `pkill -f sshd`) during an upload → the status bar reports the closed
      connection, the tab shows **Connection lost** plus **Close tab**, and
      closing it removes the tab without losing other sessions.
## 18. Edit a remote file and upload on save — [U]

**How:** double-click a remote file (or right-click → **Open with…**), pick an
editor and leave **Upload changes automatically when the file is saved** ticked
(it is on by default and remembered per session). The file is downloaded once to
a temp folder and opened; every save in the editor is uploaded back to the same
remote path.

**Behaviour**

- The editor starts only after the download finishes; the status bar says
  `Opened notes.txt in Zed — saving uploads it back`.
- Saves are detected with **filesystem events** (`notify`/inotify): the folder
  containing the temp copy is watched, so write-and-rename saves are caught too.
  Events are debounced for 150 ms and confirmed by size+mtime before uploading,
  so one `:w` produces exactly one upload. A 5-second safety pass covers events
  inotify can miss; there is no 600 ms polling loop.
- The upload uses the normal transfer queue: the transfer row shows
  `Uploading notes.txt…`, the status bar ends with `Uploaded …/notes.txt`, and
  the listing refreshes.
- The upload writes `notes.txt.part` on the server and renames it on success, so
  a failed upload never leaves a truncated remote file.
- Saving twice in a row queues both uploads; the queue runs them in order, so
  the newest content wins.
- Unticking the checkbox opens the file without watching it, and the status bar
  names the temp copy instead.
- **Offline edits are kept for 15 minutes.** If SFTP or SSH drops while a file
  is open, the local save still happens. On **⚡ Reconnect** (or on a new session
  to the same `user@host:port`), every copy whose local contents differ from
  what the server last received is uploaded automatically with the status note
  `changes kept from before the disconnect`. Re-opening that file before
  reconnecting opens the local copy with its changes instead of downloading over
  them. After 15 minutes the pending entry is dropped with a status message.
- A full SSH loss still ends the tab (**Connection lost** + **Close tab**); the
  journal is handed to the next session when that tab is closed.

**Verify**

- [ ] **Edit and save**: open `notes.txt` in gvim, append a line, `:w` → the
      status bar says `Uploaded /…/notes.txt` almost immediately and `cat` in
      the terminal shows the new line.
- [ ] **Rapid saves**: change and save twice quickly → both uploads appear in
      order and the remote copy ends with the second version.
- [ ] **Same-size save**: replace one word with a word of the same length, save
      → the upload still happens (the timestamp changed).
- [ ] **No auto-upload**: untick the checkbox, edit and save → no upload; the
      remote file is unchanged and the status bar only names the local copy.
- [ ] **Failed upload**: remove write permission on the remote file
      (`chmod 444`) and save → the status bar shows the error, the remote file
      is untouched (no `.part` left) and the session stays connected.
- [ ] **Offline edit**: with `notes.txt` open, click ⚡ **Reconnect**, edit and
      save while the channel is down, then reconnect within 15 minutes →
      the change is uploaded automatically.
- [ ] **Reconnect in a new tab**: stop sshd while editing, close the dead tab,
      start sshd, connect again with the same profile → the pending change is
      uploaded.
- [ ] **Expiry**: leave a pending file unsynced for more than 15 minutes → the
      journal drops it and the status bar says so.

## 19. Settings and remote display (X11 / VNC) — [U]

**Settings** is a dedicated window opened from the tab bar. It has three
sections:

- **Appearance** — the theme (§15).
- **File panel** — *Show Back/Forward history buttons* (off by default).
- **Remote display** — *X11 forwarding* and *VNC over SSH*, both **off** by
  default, plus a preference and the VNC fallback address.

The preference is used only when both options are enabled: **Ask every time**
shows a two-button chooser when you click **Display**, **Prefer X11** tries X11
first and falls back to VNC, and **Prefer VNC** does the reverse. Every setting
is saved immediately to `settings.toml`; old files keep the defaults for fields
they do not contain.

### X11 (`ssh -X`, untrusted)

When *X11 forwarding* is on, the request is sent **before the shell starts**, so
the remote shell and every program it launches inherit `DISPLAY`. The server's
answer is checked: a refusal (X11Forwarding off) is reported as
`X11: unavailable` in the bottom bar with a tooltip explaining why. There is no
local listening socket; the app only forwards `x11` channels that the server
opens, which is what makes it untrusted forwarding. The first X11 packet is
rewritten with the local `xauth` cookie, so the remote side never sees the real
cookie.

Enabling the option affects the **next connection**; an existing tab was
connected without the request, and the bar says so.

### VNC over SSH

When *VNC over SSH* is on, **Display** looks for a VNC session on the server,
in tiers:

1. **`vncserver -list`** — the sessions belonging to your own account
   (TigerVNC, TightVNC and TurboVNC all provide this). The lowest display
   number wins, e.g. `:1` → port 5901.
2. **Create one** — if no session exists and *Create a VNC session if none
   exists* is ticked (on by default), `vncserver` is run and the new display
   is used. The server needs `~/.vnc/passwd`; if it is missing, rustterm says
   so instead of hanging — run `vncpasswd` on the server once.
3. **Any listener** — `ss -ltn || netstat -ltn || netstat -an` finds a VNC
   port in 5900–5999 even without the `vncserver` wrapper. The bind address
   is kept, with wildcard addresses mapped to `127.0.0.1`.
4. **The editable address** — if nothing was found, the **VNC server** field
   is used (`host` or `host:port`; blank means `127.0.0.1:<port>`).

Every command runs through `/bin/sh`, so it works when the login shell is csh
and on very old systems.

The tunnel is a local loopback listener whose connections are opened with
`direct-tcpip` to the server. The bottom bar shows **both** ends:

```
VNC :1 → 127.0.0.1:37941
```

`:1` is the remote display the tunnel points at; `37941` is the local port a
viewer connects to (the OS picks it, so it is not necessarily in the 5900
range). The bar has **Copy** (copies the local address, for a viewer on
another machine), **Open**, **Stop** and **Run on :1**.

The external viewer is the first of `vncviewer`, `xtigervncviewer`,
`gvncviewer`, or `remmina -c`. If none is installed, the tunnel stays up and
the bar shows the copyable address (and notes that X11 is active when it is).

**Run on :N** types the login shell's own way of setting `DISPLAY`. The VNC
probe reads the login shell from `/etc/passwd` (falling back to `$SHELL`):

- **bash / zsh** — `DISPLAY=:N `, which applies to the next command:
  ```
  DISPLAY=:1 gvim notes.txt
  ```
- **csh / tcsh** — `setenv DISPLAY :N; `, which persists for the rest of the
  shell. Type a command after it, or press Enter on its own:
  ```
  setenv DISPLAY :1; gvim notes.txt
  ```
- **Anything else, or detection failed** — `env DISPLAY=:N `, which applies to
  the next command.

`DISPLAY=:N ` is bash/zsh syntax and csh rejects it; `env DISPLAY=:N` on its own
sets the variable only for that one `env` call, so it does not survive to the
next command in csh. That is why csh/tcsh get `setenv`.

It is only typed — nothing is launched for you, and you can edit or delete it
before pressing Enter. It is disabled while a full-screen program (vim,
htop) is running, because the text would go to that program instead of the
shell.

**Stop** closes the listener, the SSH channels and the viewer process, and
clears the bar. Closing the viewer does the same automatically ("Viewer closed
— VNC tunnel stopped"); closing the tab does too.

The VNC password is **not** your SSH password. The viewer's prompt asks for
the password set by `vncpasswd` on the server.

**Verify**

- [ ] **Off by default**: a fresh `settings.toml` has `x11 = false` and
      `vnc = false`; no Display button appears in the bottom bar.
- [ ] **Old file**: a `settings.toml` containing only `theme = "graphite"` loads
      with all new fields at their defaults and does not error.
- [ ] **X11 accepted**: enable X11, reconnect, run `echo $DISPLAY` and
      `xeyes`/`xclock` on the server → the window appears locally and
      `X11: forwarding` (or `N connections`) is shown.
- [ ] **X11 refused**: set `X11Forwarding no` on the server, reconnect →
      `X11: unavailable` with the server's reason; the terminal still works.
- [ ] **X11 enabled mid-session**: enable it without reconnecting, click
      **Display** → the bar explains the session must be reconnected.
- [ ] **VNC detection**: start `vncserver :1` on the SSH host, enable VNC,
      click **Display** → the bar shows `VNC :1 → 127.0.0.1:<port>` and the
      viewer opens.
- [ ] **VNC reuse**: click **Display** twice → the same display is reused; no
      second session is created.
- [ ] **VNC auto-create**: with no session running, click **Display** → a new
      display is created and the bar shows its number.
- [ ] **VNC auto-create off**: untick *Create a VNC session if none exists*;
      with no session running, click **Display** → no session is created and
      the editable address is used.
- [ ] **Missing vncpasswd**: remove `~/.vnc/passwd` on the server, click
      **Display** → the bar says to run `vncpasswd`; nothing hangs.
- [ ] **VNC fallback address**: stop the VNC server (nothing listens) and set
      the editable address to a host that does → the tunnel uses the setting.
- [ ] **No viewer**: temporarily hide the viewers from `PATH`, click
      **Display** → the bar shows `VNC tunnel ready at 127.0.0.1:<port>`;
      **Copy** copies it; with X11 active the message says so.
- [ ] **Run on :N**: in a bash/zsh login, click **Run on :1** → `DISPLAY=:1 `
      appears at the prompt; type `xclock` and press Enter → the window opens in
      the VNC viewer. In a csh/tcsh login the button types
      `setenv DISPLAY :1; ` instead; type `xclock` → the same result, and later
      GUI commands also use `:1` until you `unsetenv DISPLAY`.
- [ ] **Both enabled, Ask**: click **Display** → the chooser appears; **X11**
      and **VNC** each do the right thing; **Cancel** closes it.
- [ ] **Prefer X11 / Prefer VNC**: with both enabled, the matching path starts
      without the chooser, and an unavailable X11 falls back to VNC.
- [ ] **Stop**: click **Stop** → the viewer closes, `ss -ltn` on the local
      machine no longer shows the port, and the bar forgets the address.
- [ ] **Viewer exit**: close the viewer window → the tunnel stops by itself and
      the bar clears.

