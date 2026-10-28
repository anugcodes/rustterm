# TESTING — manual test sessions

rustterm is a GUI app with no accessibility tree, so pixel automation is
brittle. This file is the **manual procedure**: exact steps, expected results,
and what to capture when something fails. `USAGE.md` explains how features work;
this file is how to prove they still do.

Use it in two passes:

- **Smoke pass (≈10 min)** — run before merging anything that touches UI, SSH,
  SFTP or themes.
- **Full pass (≈40 min)** — run before a release or after a dependency bump.

Legend: `[ ]` not run · `[x]` pass · `[!]` fail (record the ID in §3).

---

## 0. Setup (once per session)

### 0.1 Build and run with logs

```sh
cd ~/Desktop/VsTerm
cargo build
mkdir -p target/manual
RUST_LOG=info cargo run 2>&1 | tee "target/manual/run-$(date +%Y%m%d-%H%M).log"
```

The log captures font loading, tab-title changes and the bell. Keep the path; it
is the first thing to attach to a failure report.

### 0.2 Test server

```sh
ss -ltnp | grep ':22'      # sshd must be listening
getent passwd sshuser      # disposable test user must exist
```

If either is missing, create the server and user as described in
`handoff.md` §5. Use `sshuser@localhost:22` for everything below.

### 0.3 Profile under test

On the **New Session** tab, load the saved `sshuser` profile (row **Load**).
The Host, Port and User fields fill in and the password box shows dots.

If the profile does not exist, create it once: Host `localhost`, Port `22`,
User `sshuser`, type the password, tick **Save password in the system keyring**,
tick **Save as profile**, name it `sshuser`, press **Connect**. From then on the
password comes from the keyring.

### 0.4 Optional helpers

Needed only for the checks noted below; skip those checks if missing.

```sh
sudo apt install -y xclip libsecret-tools   # clipboard + keyring inspection
```

### 0.5 Recording results

Annotate the boxes directly, or copy the summary block from §4. Never paste the
password into a report; it is not stored in any config file, but keep it out of
logs anyway.

---

## 1. Smoke pass

| ID | Do | Expect |
|----|----|--------|
| S1 | Start the app. | **New Session** tab is active; one card with Host/Port/User/Password; saved-profiles table below; **🎨 Theme** at the far right. |
| S2 | Click **Load** on the `sshuser` row. | Host `localhost`, Port `22`, User `sshuser`; password box shows dots; **Save password in the system keyring** is ticked. |
| S3 | Click **Connect**. | The tab label becomes `sshuser@<host>:~`; a shell prompt appears; the SFTP pane lists `/home/sshuser`; the bottom bar shows **Disconnect**. |
| S4 | Type `whoami` ↵, `pwd` ↵, `echo hello from rustterm` ↵. | Outputs: `sshuser`, `/home/sshuser`, `hello from rustterm`. No duplicated or missing characters. |
| S5 | Click **+ New Session**, load the profile, **Connect**. In the new tab run `sleep 30`; switch back to the first tab and run `echo tab1`. | Two tabs with independent output; `echo tab1` runs immediately in tab 1 while tab 2 is still sleeping. |
| S6 | Run `seq 1 500`; scroll up with the wheel; press `Shift+Home`. | A scrollbar thumb appears on the right edge; you can reach line 1; `Shift+End` returns to the prompt. |
| S7 | Run `stty size`; drag a window edge, then run `stty size` again; open `htop`, resize, then `q`. | The two numbers change; `htop` reflows instead of clipping. |
| S8 | Run `printf '\033]0;My Title\007'`. | The tab label changes to `My Title`. |
| S9 | Run `printf '\a'`, then check the log. | The log contains a `terminal bell` line. |
| S10 | Click **Disconnect** in the bottom bar. | The tab closes; `ss -tnp \| grep ':22'` shows no socket owned by rustterm. |
| S11 | Open **🎨 Theme**, choose **Graphite**; quit and relaunch. | The whole UI is recoloured immediately; Graphite is still selected after restart. Switch back to **Midnight**. |
| S12 | Quit, relaunch, **Load** the profile. | Fields are restored and the password still comes from the keyring. |

If any smoke item fails, stop and report before running the full pass.

---

## 2. Full pass

### 2.1 Connection errors

- [ ] **C1 — refused port.** Host `localhost`, Port `1`, User `sshuser`, Connect.
      Expect an inline red error in the card, the button usable again, and no tab
      opened. Complete within ~5 s; no freeze.
- [ ] **C2 — bad password.** Duplicate the profile under a new name, clear the
      password box, type a wrong password, untick keyring storage, Connect.
      Expect an inline auth error, no tab. Delete that profile afterwards.
- [ ] **C3 — empty fields.** Clear Host and User, Connect. Expect a specific
      inline message naming the missing field; no connection attempt.
- [ ] **C4 — one attempt at a time.** Connect to an unroutable host
      (`10.255.255.1`, port 22). While it is trying, the Connect button is
      disabled/spinning. No second tab appears.

### 2.2 Profiles and keyring

- [ ] **P1 — metadata only on disk.** Create profile `manual-smoke` with the
      keyring box ticked, connect, then:
      `grep -i -E 'password|secret' ~/.config/rustterm/sessions.toml`
      → no match. The file has name/target only.
- [ ] **P2 — load restores the secret.** Restart, **Load** `manual-smoke`.
      The password box is filled; Connect succeeds.
- [ ] **P3 — delete removes the secret.** Delete the profile with the row's `×`,
      then inspect the keyring (Seahorse → `rustterm`, or
      `secret-tool search service rustterm`). The entry is gone.
- [ ] **P4 — name required for keyring.** Tick the keyring box, leave the profile
      name blank, Connect. Expect
      `Profile name is required when saving the password`; nothing is written.
- [ ] **P5 — same name updates.** Save a profile twice with the same name and a
      changed port. Expect one row, updated in place, no duplicate.
- [ ] **P6 — missing keyring entry.** With `manual-smoke` saved, clear its keyring
      entry via Seahorse, restart and **Load**. Expect an inline error and an
      empty password box; typing the password manually still connects.

### 2.3 Host-key verification (TOFU)

These edit `~/.ssh/known_hosts`, so back it up first and restore at the end.

```sh
cp ~/.ssh/known_hosts ~/.ssh/known_hosts.manual-test.bak
ssh-keygen -R localhost
```

- [ ] **H1 — reject.** Connect. A modal shows the fingerprint. **Reject** →
      connection aborts; `ssh-keygen -F localhost` finds nothing.
- [ ] **H2 — accept and save.** Connect again, **Accept & save** → the session
      opens; `ssh-keygen -F localhost` now finds the key.
- [ ] **H3 — changed key.** Edit the stored line in `~/.ssh/known_hosts`
      (change one character of the base64 key) and connect. Expect a hard
      failure with no bypass button.
- [ ] **H4 — restore.** `mv ~/.ssh/known_hosts.manual-test.bak ~/.ssh/known_hosts`.

### 2.4 Terminal behaviour

- [ ] **T1 — interrupt.** Run `sleep 100`, press bare `Ctrl+C`. The command is
      interrupted; nothing is copied.
- [ ] **T2 — literal ^V.** Press bare `Ctrl+V`. A literal `^V` appears; no
      paste happens.
- [ ] **T3 — copy/paste.** Run `echo hello world`; drag-select `hello world`
      (blue highlight); `Ctrl+Shift+C`; then `xclip -o -selection clipboard` in
      another terminal → `hello world`. Paste with `Ctrl+Shift+V` → text appears
      at the prompt but does not run.
- [ ] **T4 — right-click menu.** With no selection, **Copy** and **Clear
      selection** are greyed out. **Select All** highlights the screen; **Clear
      selection** removes the highlight.
- [ ] **T5 — alt-screen copy.** Open `vim`, select text, copy it; exit `vim` and
      confirm the highlight is gone.
- [ ] **T6 — 256 colours.** `printf '\033[38;5;196mred\033[0m normal\n'` renders
      red then normal text; `ls --color=always` shows coloured entries.
- [ ] **T7 — mouse and cursor keys.** In `htop`, the wheel scrolls the process
      list; in `vim`, arrow keys move the cursor (application cursor mode).
- [ ] **T8 — known limitation, not a bug.** Pasting three lines into the shell
      executes them one by one (bracketed paste is not implemented, F15).
      Record the behaviour; do not file it as a new bug.

### 2.5 Scrollback

- [ ] **SC1 — wheel.** `seq 1 500`, scroll up; the thumb appears and tracks.
- [ ] **SC2 — keys.** `Shift+PgUp`/`Shift+PgDn` page; `Shift+Home`/`Shift+End`
      jump to the ends.
- [ ] **SC3 — alt-screen.** In `vim`, the wheel does not scroll rustterm's
      history (the app scrolls instead).
- [ ] **SC4 — after resize.** Scrollback still works after changing the window
      size; no garbled lines.

### 2.6 Resize and layout

- [ ] **R1 — PTY size.** `stty size` changes after a window resize.
- [ ] **R2 — full-screen app.** `htop` reflows while resizing; `q` exits cleanly.
- [ ] **R3 — small window.** At ~700×400 the New Session page scrolls
      vertically; nothing is clipped and no horizontal scrollbar appears.
- [ ] **R4 — wide window.** At ~1600 px the form column stays centred and does
      not stretch.

### 2.7 SFTP panel

Use a scratch directory so nothing real is touched.

- [ ] **F1 — initial listing.** After Connect the pane opens at `/home/sshuser`,
      directories first, then files; headers are Name/Owner/Modified/Size.
- [ ] **F2 — hidden files.** Toggle **👁 Hidden**; dotfiles appear; the selection
      clears if the selected row becomes hidden.
- [ ] **F3 — path box.** Type `/tmp` and press Enter; the listing changes.
- [ ] **F4 — new folder.** Go **🏠 Home**, click **➕ New folder**, create
      `sftp-test`, then double-click it to enter.
- [ ] **F5 — refresh.** In the terminal run `touch a.txt b.txt`, then click
      **🔄 Refresh**; both files appear.
- [ ] **F6 — rename.** Right-click `a.txt` → **Rename…** → `c.txt`. The row
      updates; `/` in the name is rejected.
- [ ] **F7 — copy/paste.** Select `c.txt`, **🗐 Copy**, then **📋 Paste**.
      A `c (copy).txt` appears; a second paste gives `c (copy 2).txt`.
- [ ] **F8 — delete.** Right-click `b.txt` → **Delete…**; `Esc` cancels. Repeat
      and confirm; the file disappears. The modal names the file and warns that
      it cannot be undone.
- [ ] **F9 — sizing.** Drag a header divider to resize a column; drag the panel
      edge (clamped to 340 px…55 %, max 720 px). The terminal stays visible.
      Switch tabs → each tab keeps its own width and directory.
- [ ] **F10 — open with.** Double-click a file → the **Open with…** modal lists
      editors; entries not on `PATH` are disabled. Cancel.
- [ ] **F11 — messages.** The ☰ button shows the last 50 messages with
      timestamps; **Clear messages** empties it.
- [ ] **F12 — hide/show.** **Files** in the tab bar and the panel's `×` hide it;
      the terminal reflows. Showing it restores the previous directory.
- [ ] **F13 — cleanup.** In the terminal: `rm -rf ~/sftp-test`; **🔄 Refresh**.

### 2.8 Appearance

- [ ] **A1 — switch.** Midnight ↔ Graphite recolours the tab bar, cards, tables,
      dialogs and status bar immediately; no unreadable text.
- [ ] **A2 — persistence.** The choice survives a restart.
- [ ] **A3 — terminal independent.** `ls --color=always` colours do not change
      when the UI theme changes.

### 2.9 Tab title and bell

- [ ] **B1 — title.** `printf '\033]0;My Title\007'` sets the tab label; a new
      shell prompt may restore it.
- [ ] **B2 — bell.** `printf '\a'` logs `terminal bell`; no audible beep is
      expected yet.

### 2.10 New Session page details

- [ ] **N1 — conditional fields.** The key-path field appears only after
      **Use a private key**; the profile-name field is disabled until **Save as
      profile** is ticked.
- [ ] **N2 — empty connect.** Connect with everything blank → an inline message
      explains what is missing; the button becomes usable again.
- [ ] **N3 — profile count.** The saved-profiles heading shows the correct count;
      hovering it reveals the store path.

### 2.11 File transfers

- [ ] **T1 — download file.** Select a remote file, click **⬇**, choose a local
      folder → progress bar advances, status says `Downloaded …`, and
      `cmp` reports the local copy identical. No `.part` file remains.
- [ ] **T2 — download folder.** Right-click a folder with sub-folders and a
      symlink → **Download…** → the tree is recreated locally and the symlink
      still points at the same target.
- [ ] **T3 — upload.** Click **⬆**, pick two files → both appear in the remote
      listing after the automatic refresh.
- [ ] **T4 — upload into folder.** Right-click a sub-folder → **Upload folder
      here…** → the tree appears inside that folder, not in the current one.
- [ ] **T5 — drag & drop.** Drop a file from the file manager onto the panel →
      it uploads into the visible directory.
- [ ] **T6 — conflict.** Upload an existing file → the modal appears. **Replace**
      overwrites it; **Keep both** creates `name (copy)`; **Cancel** and `Esc`
      change nothing.
- [ ] **T7 — queue and cancel.** Start two large downloads → the status bar
      shows `+1`; press **×** during the first → `Transfer cancelled`, the
      `.part` file is gone and the second transfer starts.
- [ ] **T8 — upload a PDF.** Upload a large scanned PDF (incompressible). The
      status bar advances, `ls -l` matches the local size, `cmp` succeeds, and
      the session stays connected. This is the regression that used to end with
      `sender dropped` / a silent tab.
- [ ] **T9 — upload a mixed folder.** Upload ~100 MB of text, PDFs and images.
      The terminal still echoes input while the folder is scanned, every file
      arrives, and the connection survives.
- [ ] **T10 — connection loss.** Stop sshd during an upload → the status bar
      reports the closed connection and the tab shows **Connection lost** with
      **Close tab**. The other tabs keep working.
- [ ] **T11 — cleanup.** In the terminal: `rm -rf ~/sftp-test ~/Downloads/rt-*`.

---

## 3. When something fails

Record, in this order:

1. **ID and step** (for example `S7`), what you did, what you expected, what
   happened. Note whether it reproduces from a fresh launch.
2. **Screenshot** of the whole window (and a crop of the broken area).
3. **Log**: `target/manual/run-*.log` from the same session. For a crash, rerun
   with `RUST_BACKTRACE=1 RUST_LOG=debug cargo run`.
4. **Connection state**: `ss -tnp | grep ':22'`.
5. **Profile state**: `~/.config/rustterm/sessions.toml` (metadata only — the
   password is never in it) and the theme file `~/.config/rustterm/settings.toml`.
6. **Keyring**: Seahorse → `rustterm`, or `secret-tool search service rustterm`
   when `libsecret-tools` is installed.

Do not include the account password in any report, screenshot or log excerpt.

---

## 4. Summary block

Copy this into the issue or commit message:

```text
Run date:
Build:            (git rev-parse --short HEAD, dirty?)
Smoke:            S1–S12: __ / 12
Connection:       C1–C4:  __ / 4
Profiles/keyring: P1–P6:  __ / 6
Host key:         H1–H4:  __ / 4
Terminal:         T1–T8:  __ / 8
Scrollback:       SC1–SC4:__ / 4
Resize:           R1–R4:  __ / 4
SFTP:             F1–F13: __ / 13
Appearance:       A1–A3:  __ / 3
Title/bell:       B1–B2:  __ / 2
New Session:      N1–N3:  __ / 3
Failures:         (IDs + one line each)
Artifacts:        target/manual/run-*.log, screenshots
```

---

## 5. Cleanup after a session

- [ ] Disconnect every tab (or quit; each session disconnects cleanly).
- [ ] Delete temporary profiles (`manual-smoke`, the bad-password copy).
- [ ] Remove their keyring entries via Seahorse.
- [ ] `rm -rf ~/sftp-test` on the server.
- [ ] Restore `~/.ssh/known_hosts.manual-test.bak` if you ran §2.3.
- [ ] Theme back to **Midnight** if you changed it.
- [ ] Keep the log until the change is merged, then delete `target/manual/`.

---

## 6. What is out of scope

- **File transfers** (upload/download, progress, queue, drag & drop) are not
  implemented yet; see the F-list in `handoff.md`.
- **Accessibility tree / screen readers**: eframe is built without accesskit, so
  there is nothing to inspect.
- **Bracketed paste** (F15), **double-click word selection**, and terminals
  smaller than 900 px with the SFTP panel open are known gaps, not regressions.
