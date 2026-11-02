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
| D6 | Low | [x] | `main.rs:19` | Font path `assets/JetBrainsMono-Regular.ttf` does not exist in the repo; every start warned and fell back, and the relative path depended on CWD. | Tries the CWD and executable-directory copies; a missing font is logged at debug level only. |
| D7 | Low | [x] accepted | `x11.rs:58` | `X11Config` is used by `X11Shared::config/set_config` and `ssh.rs:1170`. | **Accepted:** not dead. |

## B. Correctness and security

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| B1 | High | [x] | `sftp.rs:174-214,1432,1481` | On a successful `UploadOne`, the journal re-read the *current* `FileStamp`. A save during the upload was recorded as already uploaded and never retried. | `ActiveTransfer` now stores the stamp captured at enqueue; success marks only that version. Regression test added. |
| B2 | High | [x] | `app.rs:1180-1197` | Reconnect replaced `session.sftp` with a fresh explorer and dropped `EditJournal`; `close_session` preserves it via `detached_edits`, reconnect did not. | Reconnect now takes the old journal, starts the new explorer with the same profile key, and installs it (which triggers `sync_journal`). Test covers the detach contract. |
| B3 | High | [x] | `ssh.rs:44-52,1352,1484`, `app.rs:1429` | Session output used an unbounded channel; the per-frame drain budget was shared across all tabs, so a flooding background tab grew memory without bound and starved the visible tab. | Session reading pauses at a 2 MiB per-tab high-water mark (no bytes dropped; the SSH window provides backpressure). Each tab gets its own drain budget and the visible tab is drained first. |
| B4 | High | [x] | `ssh.rs:2834` | If rename failed and the target existed, the target was deleted before the retry. If the retry failed, the original remote file was gone. | `replace_remote_file` moves the old file aside and restores it on failure; never deletes first. |
| B5 | Med | [x] | `app.rs:3803+` | The VNC tunnel was torn down the moment the spawned viewer exited. Viewers that daemonise or hand off (`remmina -c`) exit immediately, killing the tunnel. | Teardown now waits `VNC_TUNNEL_GRACE` (15 s) after the viewer exits with no connected client; a handoff client cancels it. |
| B6 | Med | [x] | `sftp.rs:1388` | `cancel_transfer` cancelled the running transfer but left the queue; after the cancel error, `poll_transfer` started the next queued item. The docstring and the button both imply cancel-all. | Clears the queue even while a transfer is running; regression test added. |
| B7 | Low | [x] | `x11.rs:91+`, `ssh.rs:1186` | `local_auth` ran `xauth` through blocking `std::process::Command` on an async task; `encode_hex` allocated with `format!` per byte. | Caller uses `tokio::task::spawn_blocking`; `encode_hex` writes into one pre-sized `String`. |
| B8 | Low | [x] | `x11.rs:30-90` | `Display::parse` mishandled `host/unix:0`; auth matching compared only the display number, ignoring host/screen; `6000 + number` could overflow `u16`. | Shared parser handles host/transport/number/screen; auth entries must match host+number and any named screen; TCP port uses `checked_add`. Tests added. |
| B9 | Low | [x] | `ssh.rs:1820` | The VNC loopback listener spawned an unbounded task per accepted connection. | At most `VNC_MAX_CLIENTS` (8) concurrent viewers; excess sockets are closed immediately. |

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
| S1 | Low | [x] | `persist.rs`, `settings.rs:171`, `sessions.rs:195` | Writes used a fixed `.tmp` sibling and no fsync: concurrent instances could collide, and a crash could lose the write. | Shared `atomic_write` uses a pid+counter temp name, `sync_all`, then rename. Tests cover replacement and temp cleanup. |
| S2 | Low | [x] | `sessions.rs:224` | A malformed `sessions.toml` was reported but then overwritten wholesale on the next save. | `preserve_malformed` copies it to `sessions.toml.bak` before writing; test added. |
| S3 | Low | accepted | `ssh.rs:2605` | `is_connection_loss` / `is_sftp_timeout` / `explain_connect_error` classify by substring. | **Accepted:** the libraries do not expose typed errors at our boundary; behaviour is covered by tests. Documented in code. |
| S4 | Low | accepted | `secrets.rs` | Keyring passwords are never zeroized (handoff F19). | **Deferred:** zeroizing our copy is ineffective while `russh` also holds the password; needs a cross-cutting secret type, not a surgical fix. |

## E. Docs and CI

| ID | Sev | Status | Location | Issue | Fix |
|----|-----|--------|----------|-------|-----|
| E1 | Med | [x] | `README.md:9-17` | Status paragraph still said transfers and follow-`cd` were future work. | Rewritten to describe the shipped feature set. |
| E2 | Med | [x] | `README.md:48-51` | Four shipped features were unchecked. | Follow-`cd`, drag-and-drop, edit/auto-upload and X11/VNC are ticked. |
| E3 | Low | [x] | `docs/archive/PLAN-ui-overhaul.md` | Completed plan still sat at the root. | Moved with `git mv` and marked archived. |
| E4 | Low | [x] | `handoff.md:693` | No roadmap entry for the planned module split. | Added `Post-MVP` item 14 with split order and deferred perf work. |
| E5 | Med | [x] | `.github/workflows/ci.yml` | CI ran only `fmt`, `clippy` (no `--all-targets`) and `build`; no tests, no `--no-default-features`. | CI now runs `clippy --all-targets`, `test`, `test --no-default-features` and `build --no-default-features`. |

## Accepted code patterns (no change)

- `terminal.rs` `input_tx.lock().unwrap()`: the mutex is never held across a
  panic path; recovering from poisoning would hide real bugs.
- `app.rs:1185,1250` `expect("checked above")`: guarded immediately above by a
  `take()`; left as-is with a clarifying comment.
