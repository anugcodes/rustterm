# Plan: performance profiling and module restructuring

Status: **not started** — successor to `AUDIT.md` (all items resolved).

## Goal

1. Measure before changing anything. Any perf work must be justified by a
   profile, not by inspection.
2. Split the two large modules (`ssh.rs`, `sftp.rs`) into cohesive units
   while keeping the full gate green after every move.
3. Fold in the deferred performance work (P3, P4, D3) *while* moving code,
   not as a separate pass.

## Baseline (record first)

- `cargo test` → 123 tests passing.
- `cargo build --release` time and binary size.
- Run the app under a profiler while exercising:
  - `cat` of a 100 MiB file in the terminal (terminal render path).
  - A 10 000-file SFTP listing, scroll + sort by every column.
  - Recursive upload/download of a 1 000-file tree.
  - VNC tunnel with a real viewer.
- Capture flamegraphs / perf samples into `docs/profiling/` (not committed
  binaries, just reports).

## Performance profiling steps

1. **Instrument, don't guess.** Add `tracing` spans around the hot paths
   (`terminal.rs` render, `sorted_indices`, `download_file_inner`,
   `upload_file_inner`) and run with `RUST_LOG=trace`.
2. **Terminal render (P3).** Profile `LayoutJob` construction per row. Decide
   between (a) a dirty-region cache keyed by (row, terminal revision) and
   (b) incremental layout. Cache is preferred only if invalidation is simple;
   otherwise keep full rebuild and document why.
3. **SFTP sort (P1 — done).** Already replaced `to_lowercase()` with
   `cmp_lower`. Verify with a profile that no allocation remains.
4. **Clones (P4).** Run `cargo clippy -- -W clippy::large_enum_variant` and a
   manual pass. Only remove clones that a profile shows as hot; do not
   churn ownership for cleanliness.
5. **Join path (D3).** Move `sftp::join_path` into a tiny shared helper
   (`src/path.rs` or `crate::util`) and call it from both sites. Done during
   the `sftp.rs` split.

## Restructuring sequence (each step = one PR, full gate green)

### Step A — `ssh.rs` (≈3.9k lines) → `ssh/`

- `ssh/connect.rs` — `connect_and_auth`, host-key handling, `SshConfig`.
- `ssh/session.rs` — `run_session`, the `tokio::select!` loop, output
  backpressure (already added), channel proxying.
- `ssh/transfer.rs` — upload/download/copy helpers, `TransferProgress`,
  `ConflictPolicy`, `unique_copy_path`, `remote_temp_path`, `local_temp_path`.
- `ssh/vnc.rs` — `open_vnc`, `VncTarget`, client counting.
- `ssh/x11.rs` stays as-is (already cohesive).
- `ssh/mod.rs` re-exports the public surface; no call-site changes.

### Step B — `sftp.rs` (≈2.9k lines) → `sftp/`

- `sftp/listing.rs` — `RemoteEntry`, `SftpExplorer` listing state, sort
  helpers (moved from `app.rs`? keep in `sftp` if it owns entries).
- `sftp/transfer.rs` — queue, `ActiveTransfer`, conflict prompts, cancel.
- `sftp/edit.rs` — `EditJournal`, `EditWatch`, watcher, sync-on-reconnect.
- `sftp/mod.rs` re-exports.

### Step C — `app.rs` (≈4.2k lines) — **only if it reaches ~5–6k lines**

- Split the SFTP panel UI from the terminal/session UI. Do not split earlier;
  the cost is not worth it at the current size.

### Step D — optional

- `terminal.rs` dirty-region cache (P3) if the profile justifies it.

## Success criteria

- `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo test`,
  `cargo test --no-default-features`, `cargo build --no-default-features`
  green after every commit.
- No public API changes unless a rename is part of the move and all call sites
  are updated in the same commit.
- No new dependencies.
- A short note in `handoff.md` after each step recording what moved and any
  perf delta measured.

## Out of scope

- New features.
- Rewriting the terminal renderer.
- Changing the SSH/SFTP wire protocol handling.
