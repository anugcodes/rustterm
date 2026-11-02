# Audit findings — post-MVP

Scope: quality audit before any module split. Branch: `fix/audit-findings`.
Baseline: `c8761b9` (MVP feature-complete, 112 tests green).

Each item is ticked by the commit that fixes it. Items marked **accepted** are
deliberate decisions, not oversights; the reason is recorded with the item.

Verification gate for every commit:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --no-default-features
cargo build --no-default-features
```

## A. Dead code, duplication, meaningless paths

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| D1 | Low | [x] accepted | `ssh.rs:1849,693,1412` | `SftpAction::Reconnect` is used by `SshSession::sftp_reconnect()` and handled specially in `run_session`. | **Accepted:** not dead. |
| D2 | Low | [x] accepted | `ssh.rs:2045`, `sftp.rs:823` | `SftpAction::fail` exists and is used by the SFTP panel on session-loss paths. The dispatcher also sends errors inline where it needs to transform results. | **Accepted:** keep as-is; not worth forcing everything through the helper. |
| D3 | Low | [x] accepted | `ssh.rs:3267`, `sftp.rs:2210` | `join_remote_path` is a 9-line mirror of `sftp::join_path`. | **Accepted:** importing the UI-state module from the transport module inverts the layering; moving both into a shared helper is part of the later restructuring. |
| D4 | Low | [x] | `ssh.rs:2571` | `fetch_file` (editor download) removed the target on error instead of downloading to `.part` and renaming, so a failed fetch could destroy the user's existing editor copy. | Now streams through `download_file_inner` into a unique `.part`, then renames. |
| D5 | Low | [x] | `ssh.rs:2597+` | `.part` temp names were derived only from the target name, so a manual transfer and an editor auto-upload of the same file collided. | `remote_temp_path`/`local_temp_path` add pid + a monotonic counter. |
| D6 | Low | [ ] | `main.rs:22` | Font path `assets/JetBrainsMono-Regular.ttf` does not exist in the repo; every start warns and falls back. Relative path also depends on CWD. | Resolve candidate paths, fall back quietly, warn only at debug level. |
| D7 | Low | [x] accepted | `x11.rs:58` | `X11Config` is used by `X11Shared::config/set_config` and `ssh.rs:1170`. | **Accepted:** not dead. |

## B. Correctness and security

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| B1 | High | [ ] | `sftp.rs:465,1489` | On a successful `UploadOne`, the journal re-reads the *current* `FileStamp`. A save during the upload is recorded as already uploaded and never retried. | Stamp the exact bytes that were uploaded (`FileStamp` captured at transfer start) and mark that stamp. |
| B2 | High | [ ] | `app.rs:1180-1197` | Reconnect replaces `session.sftp` with a fresh explorer and drops `EditJournal`; `close_session` preserves it via `detached_edits`, reconnect does not. | Carry the journal across reconnect and re-sync. |
| B3 | High | [ ] | `ssh.rs:1358`, `app.rs:1429` | Session output uses an unbounded channel; the per-frame drain budget is shared across all tabs, so a flooding background tab grows memory without bound and starves the visible tab. | Per-session byte budget with async backpressure (pause reads at a high-water mark, never drop bytes). |
| B4 | High | [x] | `ssh.rs:2834` | If rename failed and the target existed, the target was deleted before the retry. If the retry failed, the original remote file was gone. | `replace_remote_file` moves the old file aside and restores it on failure; never deletes first. |
| B5 | Med | [ ] | `app.rs:3784` | The VNC tunnel is torn down when the spawned viewer exits. Viewers that daemonise or hand off to an already-running instance (`remmina -c`) exit immediately, killing the tunnel. | Stop only when the viewer is gone *and* no client is connected, after a grace period. |
| B6 | Med | [ ] | `sftp.rs:1383` | `cancel_transfer` cancels the running transfer but leaves the queue; after the cancel error, `poll_transfer` starts the next queued item. The docstring and the button both imply cancel-all. | Cancel the running transfer and clear the queue. |
| B7 | Low | [ ] | `x11.rs:151+` | `local_auth` runs `xauth` through blocking `std::process::Command` on an async task; `encode_hex` allocates with `format!` per byte. | Run the command via `spawn_blocking`; write hex into one pre-sized `String`. |
| B8 | Low | [ ] | `x11.rs:31-70` | `Display::parse` mishandles `host/unix:0`; auth matching compares only the display number, ignoring host/screen; `6000 + number` can overflow `u16`. | Parse the real forms, match host+number+screen, use checked arithmetic. |
| B9 | Low | [ ] | `ssh.rs:1774` | The VNC loopback listener spawns an unbounded task per accepted connection. | Cap concurrent connections (e.g. 8) and close the excess. |

## C. Performance

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| P1 | Med | [ ] | `app.rs:683` | `sorted_indices` runs every frame and its name/owner comparators call `to_lowercase()` twice per comparison: O(n log n) allocations per frame. | Precompute lowercase sort keys once per listing change. |
| P2 | Med | [x] | `ssh.rs:3057+` | `copy_file`/`fetch_file` used `tokio::io::copy` (~8 KiB) while `TRANSFER_CHUNK` is 64 KiB. | Both paths now stream in 64 KiB chunks. |
| P3 | Med | accepted | `terminal.rs:807` | The renderer rebuilds a `LayoutJob` for every visible row every frame. | **Not fixed here:** needs a profile first; a dirty-region cache is a large, risky change. Deferred to the restructuring roadmap. |
| P4 | Low | accepted | `app.rs`, `sftp.rs`, `ssh.rs` | 150+ `.clone()` calls. | **Not fixed here:** most are required by ownership; auditing without a profile would be speculative. Deferred. |

## D. Persistence and robustness

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| S1 | Low | [ ] | `settings.rs`, `sessions.rs` | Writes use a fixed `.tmp` sibling and no fsync: concurrent instances collide, and a crash can lose the write. | Unique temp name per process + `sync_all` before rename. |
| S2 | Low | [ ] | `sessions.rs` (load/save) | A malformed `sessions.toml` is reported but then overwritten wholesale on the next save. | Preserve the bad file as `sessions.toml.bak` before overwriting. |
| S3 | Low | accepted | `ssh.rs:2605` | `is_connection_loss` / `is_sftp_timeout` / `explain_connect_error` classify by substring. | **Accepted:** the libraries do not expose typed errors at our boundary; behaviour is covered by tests. Documented in code. |
| S4 | Low | accepted | `secrets.rs` | Keyring passwords are never zeroized (handoff F19). | **Deferred:** zeroizing our copy is ineffective while `russh` also holds the password; needs a cross-cutting secret type, not a surgical fix. |

## E. Docs and CI

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| E1 | Med | [ ] | `README.md:9-14` | Status paragraph still says transfers and follow-`cd` are future work. | Rewrite to match the shipped feature set. |
| E2 | Med | [ ] | `README.md:42-45` | Four shipped features are unchecked. | Tick follow-`cd`, drag-and-drop, edit/auto-upload, X11/VNC. |
| E3 | Low | [ ] | `PLAN-ui-overhaul.md` | Completed plan still sits at the root. | Move to `docs/archive/` with an archived header. |
| E4 | Low | [ ] | `handoff.md:623` | No roadmap entry for the planned module split. | Add `Post-MVP: codebase restructuring` under section 11. |
| E5 | Med | [ ] | `.github/workflows/ci.yml` | CI runs only `fmt`, `clippy` (no `--all-targets`), and `build`; no tests, no `--no-default-features`. | Add the full gate above. |

## Accepted code patterns (no change)

- `terminal.rs` `input_tx.lock().unwrap()`: the mutex is never held across a
  panic path; recovering from poisoning would hide real bugs.
- `app.rs:1185,1250` `expect("checked above")`: guarded immediately above by a
  `take()`; left as-is with a clarifying comment.
