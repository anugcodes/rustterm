# Changelog

## [1.0.0] - 2026-11-04

First stable release. Everything in the MVP and V2 milestones is complete; the
SFTP panel ships the 1.0 UX set (multi-select, permissions column, upload
menu, bundled terminal font).

### Added
- Permissions column in the SFTP table (`drwxr-xr-x`-style mode string, `—`
  when the server omits attributes).
- Multi-select in the SFTP table: Ctrl-click toggles, Shift-click selects a
  range, arrow keys move the selection. Bulk **Download**, **Delete** and
  **Open with…** (open only when every selected row is a regular file).
- **Upload files…** / **Upload folder…** menu under the upload icon.
- Follow is a sticky `🎯` icon toggle in the toolbar; Reconnect moved into the
  status bar.
- Bundled JetBrains Mono Regular as the terminal font (embedded, SIL OFL 1.1).
- SFTP table widened to 560 px; side panel minimum reduced to 260 px.

### Changed
- `src/ssh.rs` split into `src/ssh/{mod,connect,session,transfer,vnc}.rs`.
- `src/sftp.rs` split into `src/sftp/{mod,paths,edit,transfer}.rs`.

### Documentation
- `USAGE.md` colour diagnostics section (missing prompt colour is remote shell
  config, not an emulator bug).
- `ROADMAP.md` post-1.0 plan.

### Fixed
- `ls`-style colour parsing was already correct; documented the diagnosis.