# Changelog

## Unreleased

Race-condition remediation: concurrent jjq invocations — from any mix of jj
workspaces — now serialize correctly instead of corrupting the metadata
branch or the queue.

### Fixed

- **Per-workspace lock bypass** — Locks and the run log now resolve through
  the shared repo `.jj` directory, so invocations from secondary workspaces
  contend on the same lock files instead of each getting private, useless
  ones. An unresolvable workspace pointer is a hard error rather than a
  silent fallback.
- **Concurrent pushes corrupted the metadata branch** — All metadata writes
  (sequence ID allocation, config, workspace records) flow through a single
  writer serialized by a blocking `meta` lock. Racing pushes now wait their
  turn and both succeed with unique sequence IDs; the separate `id` and
  `config` locks are removed. Writers wait up to 15 seconds (tunable via
  `JJQ_META_LOCK_TIMEOUT_MS`) and exit 3 (`LOCK_HELD`) only on timeout.
- **`run --all` spun on a held run lock** — The run lock is taken once, up
  front; a second runner exits 1 with "another queue runner is active"
  instead of busy-retrying and counting phantom item failures.
- **Missing `check_command` looped forever** — `jjq run` now reports the
  configuration error once and exits 1 instead of endlessly re-reading the
  same queue item.
- **Conflicted jjq metadata detection** — A metadata bookmark conflicted by
  out-of-band moves now produces a short, actionable error pointing at
  `jjq doctor` everywhere it is read, instead of raw jj template errors or
  silently reading one side of the conflict.
- **Landing a live working-copy commit dragged trunk** — Pushing a
  workspace's `@` left the landed commit as that workspace's live working
  copy; the next edit was snapshotted as an amendment and the trunk bookmark
  silently followed, absorbing unchecked changes (under both strategies —
  directly under `rebase`, via descendant auto-rebase of the merge commit
  under `merge`). `push` and `requeue` now park the working copy on a fresh
  empty child of the queued commit (like `jj commit` does), and `run` warns
  when a landed change is still some other workspace's working-copy commit.

### Changed

- **No process chdir** — jjq never changes its own working directory; jj and
  check commands get their working directory set per child process, so
  concurrent operations cannot trample each other's cwd.
- **Snapshot config reads** — Each command pins the metadata bookmark to a
  single commit and reads every config value from it, so a concurrent writer
  cannot skew a group of reads.
- **`jj log` filter form** — `jjq init` writes the metadata filter as
  `~ ::bookmarks(exact:"jjq/_/_")`; the bare-symbol form breaks jj's
  short-prefixes index if the bookmark ever becomes conflicted. `doctor`
  warns about the old form and shows the migration command.
- **Run preconditions are checked before the queue** — `jjq run` errors on a
  missing `check_command` even when the queue is empty, and exits 1 when
  another runner is active even if the queue is empty.
- **`status --json`** — The `running` boolean is renamed `runner_active`; a
  new `running` array lists the item(s) claimed by an active runner.
- **`clean` is guarded** — Refuses while a queue runner or a `jjq check` is
  active, and only removes the exact workspace names jjq generates, never
  user workspaces that merely start with "jjq".

### Added

- **In-flight claim tickets** — The runner renames `jjq/queue/NNNNNN` to
  `jjq/running/NNNNNN` under the run lock. `delete`, `requeue`, and `push`
  refuse to touch a live claim (or recover a stale one from a crashed
  runner); the runner verifies the claim still exists immediately before
  landing and discards the result if the item was removed. Crash recovery
  moves an orphaned claim back to its queue position — same sequence ID —
  and cleans up the leftover runner workspace.
- **`doctor` checks and `--fix`** — Detects conflicted jjq bookmarks
  (repairing the metadata bookmark via `--fix` by keeping the side with the
  highest sequence counter), validates the sequence counter, and warns about
  stale claims left by an interrupted run.
- **Concurrency regression tests** — Deterministic two-workspace concurrent
  push test, plus an `#[ignore]`-gated push storm test
  (`cargo test -- --ignored stress_concurrent_push_storm`).

## v0.2.0 — 2026-02-18

### New commands

- **`requeue <id>`** — Re-push a failed item back onto the queue. Runs a
  pre-flight conflict check before re-queuing, so you get early feedback
  if the item still conflicts.

### Features

- **Landed history in `status`** — Status now shows recently landed items
  by scanning trunk ancestors for jjq metadata, giving visibility into
  what has already been merged.
- **Conflict paths in `status`** — Failed items now display the conflicting
  file paths (e.g. `(conflicts: main.go, util.go)`), so you can see at a
  glance what went wrong.
- **Clearer run failure output** — When a run fails, jjq now prints the
  candidate change ID and concrete `jj` commands you can copy-paste to
  resolve the issue.
- **Push output improvements** — `push` now shows the repo path and trunk
  bookmark in its output.
- **Auto-configure jj log filter** — `jjq init` now automatically sets
  `revsets.log` in the repo config to hide jjq metadata from `jj log`.
  Doctor checks for the filter and suggests the fix command.
- **Empty commit skipping** — `run` now skips empty commits that add no
  changes to trunk, avoiding no-op merges.

### Bug fixes

- Fix crash-safety violation in rebase strategy success path.
- Fix `clean` command to remove all jjq workspace types.
- Fix misleading error for overflowing sequence IDs.
- Fix duplicate check numbering in `doctor`.
- Filter sentinel lines from `tail` follow mode initial content.

### Infrastructure

- You can now install via Homebrew: `brew install tap/paulsmith/jjq`
- Add x86_64-darwin builds
- E2E test speedup (~127s to ~70s) via template caching, reduced subprocess
  overhead, and environment variable optimizations.

## v0.1.0 — 2026-02-05

Initial release.
