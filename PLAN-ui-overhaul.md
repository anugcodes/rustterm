# UI overhaul plan

Status: **done**

This plan covers the SFTP side panel, the New Session page and a shared theme
for the whole application. It is being implemented in small, compiling commits.

## 1. SFTP listing as a table

- Use `egui_extras::TableBuilder` for a real table with a sticky header.
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

- `Cargo.toml` — add `egui_extras` 0.29.
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

- Live SFTP server: table at the 340 px minimum, right-click menu, recursive
  delete warning, status history during a large copy, editor launch and
  reconnect. Run the checklist in `USAGE.md` §14.
- Very small window sizes (< 900 px wide) with the SFTP panel open.
