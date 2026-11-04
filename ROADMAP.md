# Roadmap (post-1.0)

`v1.0.0` ships the MVP and V2 milestones. Items below are ordered by leverage:
settings and theming first (they unblock later work), then performance and
transfer work, then quality-of-life. Each item has a "done when" check so it can
be verified without guesswork.

## 1.1

1. **Settings UI + persistence**
   - Settings panel for font size/family, terminal ANSI palette and theme.
   - Persist through `settings.rs`; apply on start and live.
   - Done when: every option survives a restart and no terminal colour is
     hardcoded in `terminal.rs`.

2. **Theme-aware scrollbars**
   - Add `scrollbar` / `scrollbar_hover` tokens to `theme::Palette`.
   - Use them in egui's `ScrollStyle` and the terminal's custom-painted bar.
   - Done when: switching Midnight/Graphite recolours every scrollbar.

3. **Terminal performance**
   - Measure with the existing `profiling` profile, then add dirty-region
     redraw, then move VTE decoding off the UI thread.
   - Done when: idle CPU stays near zero and large output (`seq 1 200000`) does
     not stall keyboard input.

4. **Concurrent transfers (P5)**
   - Parallelise recursive uploads/downloads with `tokio::task::JoinSet`
     (4–8 files), per-file progress and cancel-all.
   - Done when: several files transfer at once and Cancel stops all of them.

5. **Permissions editor (chmod)**
   - Modal over the existing Permissions column using
     `SftpSession::set_metadata` / `FileAttributes::permissions`.
   - Done when: mode bits can be edited, applied and refreshed.

6. **SFTP QoL**
   - Rename/move across folders, in-panel search, drag reorder.
   - Done when: each lands as its own small, tested change.

## 1.2

- **`~/.ssh/config` import**: parse `Host` / `HostName` / `User` / `Port` /
  `IdentityFile` / `ProxyJump` and merge with saved profiles.
- **Macros / snippets**: per-profile command palette with variable substitution.
- **Incremental `app.rs` split**: extract the SFTP panel, tab strip and dialogs
  into modules; no big-bang rewrite.

## Hardening

- **P4 clone audit + F19 zeroization**: review `Clone` implementations that
  handle secrets and zeroize in-memory credentials.
- **Release automation**: tag-driven CI artifacts, checksums and signing.

## Packaging

- **Phase 0 (this release)**: app icon, `assets/rustterm.desktop` and
  `install.sh` for a user-local install under `~/.local`.
- **Phase 1 (next)**: tag-driven GitHub Releases, building a tarball plus a
  `curl | sh` installer and SHA-256 checksums.
- **Phase 2**: `.deb` via `cargo-deb`, then an AppImage; AUR only if there is
  demand.
- **Deferred**: Flatpak. Sandboxing complicates the SSH agent, `~/.ssh` access
  and X11 forwarding.
