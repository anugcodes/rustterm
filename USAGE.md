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
