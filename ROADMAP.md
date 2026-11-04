# Roadmap (post-1.0)

Planned for 1.1 and later. 1.0 ships the MVP and V2 milestones.

## 1.1

- **Permissions editor (chmod)**: `SftpSession::set_metadata` and
  `FileAttributes::permissions` are available in `russh-sftp 3.0.1`. The
  Permissions column already ships in 1.0; the editor is deferred here.
- **Concurrent transfers (P5)**: parallelise recursive uploads/downloads with
  `tokio::task::JoinSet` (4–8 files at a time).
- **Terminal performance**: dirty-region redraw, move the terminal to a
  background thread.
- **SFTP QoL**: rename/move across folders, in-panel search, drag reorder.

## 1.2

- **Macros / plugin support**: remote command templates and reusable snippets.
- **Settings UI**: font and colour-palette pickers.
- **`~/.ssh/config` import**.

## Later

- P4 clone audit + F19 in-memory zeroization.
- Split `app.rs` (~5k lines) into focused modules.
- Terminal font/colour selection persistence.
- **Theme-aware scrollbars**: give `Palette` a scrollbar token and have every
  scrollbar (including the terminal's custom-painted one, which currently uses a
  hardcoded grey) pick its colour from the active theme.