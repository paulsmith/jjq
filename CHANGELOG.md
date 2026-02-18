# Changelog

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
