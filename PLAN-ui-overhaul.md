# UI overhaul plan

Status: **done**

This plan covers the SFTP side panel, the New Session page and a shared theme
for the whole application. It is being implemented in small, compiling commits.

## 1. SFTP listing as a table

- Use `egui_extras::TableBuilder` for a real table with a sticky header.
  *(Replaced in the refinement pass by a hand-rolled fixed header + scroll area,
  see below.)*
- Columns: **Name** (flexible), **Owner**, **Modified**, **Size**.
- Name cells keep the folder/file/symlink icon and a tooltip with the full name.
- Owner and modified time use muted, compact text; size is right-aligned and
  folders show an em dash.
- Rows are striped, hoverable and selectable; text is elided instead of wrapped.
- Increase the default sidebar width to fit four columns while keeping the
  panel resizable with a sensible minimum.

## 2. Bottom status bar replaces the selected-file footer

- Remove the selected-file footer (path, size, owner, timestamp, Rename/Delete
  shortcuts) and the notice/error block below the path field.
- Add a compact status bar at the bottom of the SFTP panel.
- It shows the latest message, its severity, a spinner/operation label while
  busy, and a history button.
- Clicking the history button opens the recent message list with timestamps and
  a Clear action.
- Messages include filesystem operations, editor launches and connection
  issues. The status log keeps the last ~50 entries.
- The SFTP-unavailable state keeps a Reconnect action in the table area; its
  detail is also written to the status log.

## 3. Row interaction and context menu

- Single left-click selects a row.
- Double left-click opens it: folders navigate; files open the "Open with…"
  picker.
- Right-click selects the row and opens the context menu. A normal click never
  opens a menu.

Context menu:

- File: **Open with…**, **Rename…**, **Copy path**, **Delete…**
- Folder: **Open**, **Paste into folder** (when the clipboard has an entry),
  **Rename…**, **Copy path**, **Delete…**

Removed from the menu: Open in default editor, Open in editor submenu, Copy,
New folder…, Refresh. These remain reachable from the icon toolbar.

## 4. "Open with…" popup

- A modal instead of a submenu.
- Radio list: Default editor, Zed, gvim, VS Code, Desktop default, Custom
  command.
- Editors not found on `PATH` are shown disabled.
- Custom command text field for any other local editor.
- Remembers the last choice for the session.
- Buttons: Cancel and Open.

## 5. Icon toolbar

- Replace text buttons with 28 px icon buttons grouped as navigation,
  file operations and view/connection.
- Icons: Home, Up, Refresh, New folder, Copy, Paste, Hidden, Reconnect, Hide.
- Every icon has a hover tooltip describing the action.
- Disabled states are honoured for Up/Copy/Paste.
- The path field stays, with an arrow icon for Go.

## 6. Dialog polish

- One shared modal helper with consistent padding, spacing and footer buttons.
- Delete dialog: clear title, strong file name, full path, and a tinted warning
  for recursive folder deletion. Cancel plus a red Delete button.
- Rename and New folder use the same layout, with inline validation errors.
- `Esc` cancels; content is never clipped.

## 7. New Session page

- Horizontally centred column with a maximum width, inside a vertical scroll
  area.
- Title and muted subtitle.
- Connection card: Host + Port row, User, Password, compact Authentication
  subsection.
- Primary Connect button with spinner and inline status.
- Profile card: profile name and Save profile.
- Saved sessions card: rows with name and `user@host:port`, select to load,
  delete control, empty-state hint.
- Modest padding and section spacing; no oversized whitespace.

## 8. Theme system

- New `src/theme.rs` with palette, metrics and a theme registry.
- Start with two dark presets: **Midnight** (default) and **Graphite**, to prove
  the selection path.
- Apply visuals, spacing, text styles, rounding and colours globally.
- Theme picker in the top bar; the choice is persisted in
  `$XDG_CONFIG_HOME/rustterm/settings.toml`.
- Replace hard-coded UI colours with theme colours.
- Terminal ANSI colours remain independent; terminal chrome stays conventional.

## Files

- `Cargo.toml` — add `egui_extras` 0.29. *(Removed in the refinement pass; the
  table is hand-rolled.)*
- `src/theme.rs` — new theme registry, palette and widget helpers.
- `src/settings.rs` — new theme persistence.
- `src/main.rs` — register modules and apply the default theme.
- `src/app.rs` — table, icon toolbar, status bar, dialogs, new session page,
  theme picker.
- `src/sftp.rs` — status log, editor picker, custom editor command.
- `README.md`, `USAGE.md`, `handoff.md`, `CHANGES.md` — document the result.

## Result

All eight items are implemented and committed in small steps:

- `18f217d` — plan
- `a5ee2d0` — theme system + New Session page
- `c93bcd3` — SFTP table, status bar, icon toolbar, dialogs, open-with picker
- `e6ca79f`, `4e7a8b0` — status-message preservation, theme placement and
  font-safe icon glyphs

The SFTP table follows the header/click behaviour above, and the message log
keeps the last 50 entries. Fonts: only egui's bundled families are available
(Ubuntu-Light, NotoEmoji, emoji-icon-font), so the UI uses `+ 🏠 ⬆ 🔄 ➕ 🗐 📋 👁
⚡ ➡ ☰ 🎨 ✔ • ×` and avoids glyphs that render as tofu.

## Refinement pass (flat, boxy redesign)

The first pass was structurally right but visually too soft and too tall. A
second pass tightened it:

- **Table** — `egui_extras` was dropped. The listing is now a hand-rolled fixed
  header plus `ScrollArea::show_rows`, which keeps the header and rows perfectly
  aligned and lets the header paint its own column dividers. Dividers exist
  **only in the header**; dragging one resizes the column to its right. Rows no
  longer show a resize cursor. The header and the file names use the same font
  size and weight (no bold). Default widths: owner 64, modified 122, size 52,
  name minimum 80 px.
- **"Remote files" title removed** — the panel starts directly with the toolbar.
- **Panel sizing** — default 450 px, minimum 340 px, maximum
  `min(55% of the window, 720 px)`, still draggable. The terminal is always
  visible, including on a 900 px-wide window.
- **Flat theme** — `Rounding::ZERO`, no shadows, hover changes fill only (no
  accent glow), 1 px borders, floating thin scrollbars, a 26 px control height
  shared by buttons and inputs, and identical horizontal/vertical padding.
- **Input alignment** — every single-line input uses `vertical_align(Center)`
  with a `(pad, 2)` margin, so text sits on the same baseline across a row and
  labels line up in one gutter.
- **Tabs** — one box per tab with the close `×` inside it and the label centred;
  minimum width 168 px. Middle-click also closes.
- **New Session** — connection fields, auth options and the profile checkbox live
  in **one** card. **Save as profile** is a checkbox that enables the
  profile-name field (same pattern as the private-key path). Profiles are saved
  before connecting, so validation failures abort the attempt. The saved list
  became a compact **Saved profiles (n)** table with **Name / Target**, **Load**
  and a per-row **×**, with an empty-state hint. The whole page is narrower and
  scrolls at small window sizes.
- **Dialogs** — all modals use `theme::modal_frame`, a custom title bar
  (`title_bar(false)` for the SFTP dialogs) and `.default_size(vec2(width, 1.0))`
  so they size to their content. `.auto_sized()` was tried and reverted because
  egui expands it to the whole area. The host-key prompt was restyled to match.
- **Menu →** — two entries, closes on selection, no developer hint. Every
  context menu now shares one width (`theme::MENU_WIDTH`, 220 px) and compact
  rows (22 px, 1 px item spacing); long file names in the row-menu header are
  truncated with a full-path tooltip. Modals share `theme::DIALOG_WIDTH`
  (440 px), replacing the previous 400/420/440 mix. The Delete dialog no longer
  repeats "Delete / Please confirm this action" above "Delete \“name\”?".
- **Icons** — SFTP rows use `🗀` / `🗋` / `🔗` at body font size.

### Verified on screen

- Panel cap/default/minimum, hide/show preserving width, path and selection.
- Header divider resizing, full timestamps, new icons, tab close `×`, terminal
  reflow when the panel is hidden, bottom **Disconnect** bar.
- New Session at 900×700 and 700×400.
- Rename, New folder, Delete and **Open with…** dialogs (all compact and boxy).
- Host-key prompt end-to-end (reject and accept, key written to
  `known_hosts`, connection succeeds).
- Midnight ↔ Graphite switching and theme persistence across a restart.

## Verification

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings` (default and
  `--no-default-features`)
- `cargo test` (default and `--no-default-features`)
- `cargo build --release`
- Manual live-SFTP checklist: table columns, narrow panel, hidden files,
  right-click menu, double-click, dialogs, editor picker, status history,
  reconnect, errors, theme switching, new-session layout at small and large
  window sizes.

## Manual checks still pending

- Live SFTP server: recursive delete warning, status history during a large
  copy, editor launch and reconnect. Run the checklist in `USAGE.md` §14.
- Very small window sizes (< 900 px wide) with the SFTP panel open — the 900 px
  cap and the 700×400 New Session page were checked, narrower layouts were not.
