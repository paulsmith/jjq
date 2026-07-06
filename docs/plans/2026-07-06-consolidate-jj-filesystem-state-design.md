# Consolidate jjq filesystem state under `.jj/jjq/`

**Date:** 2026-07-06
**Status:** Design

## Problem

jjq scatters its filesystem state across two sibling paths in the shared `.jj`
directory:

- `.jj/jjq-locks/` — flock files (`run.lock`, `meta.lock`, `check.lock`)
- `.jj/jjq-run.log` — combined stdout/stderr of the last check command

There is no single, discoverable home for jjq's on-disk state, and no
convention for where future filesystem artifacts should live. This affects
every jjq-using repository, not just this one.

Note: jjq's *other* state is not on the filesystem and is out of scope here —
the `jjq/_/_` metadata branch, the `jjq/queue/*` / `jjq/running/*` /
`jjq/failed/*` bookmarks, and the operation log all live *inside* jj as commits
and bookmarks. Runner workspaces live in `/tmp`. Only the two paths above are
physical files under `.jj/`.

## Goal

Consolidate all jjq filesystem state under a single `.jj/jjq/` directory, and
establish that directory as the documented, future-proof home for any
filesystem state jjq adds later.

## Target layout

```
.jj/jjq/
├── locks/
│   ├── run.lock
│   ├── meta.lock
│   └── check.lock
└── run.log
```

Resolved (as today) through the *shared* repo `.jj` directory
(`jj::shared_jj_dir()`), so invocations from any workspace contend on the same
lock and log files.

Legacy paths `.jj/jjq-locks/` and `.jj/jjq-run.log` are retired.

## Design

### 1. `paths` module

Introduce `src/paths.rs` as the single place that encodes the `.jj/jjq/`
convention. All jjq filesystem paths are derived here rather than by joining
literals onto `shared_jj_dir()` at each call site.

```rust
// ABOUTME: Resolves jjq's on-disk state paths, all rooted at .jj/jjq/.

use anyhow::Result;
use std::path::PathBuf;
use crate::jj;

/// Root of all jjq filesystem state, under the shared .jj directory.
pub fn jjq_dir() -> Result<PathBuf> {
    Ok(jj::shared_jj_dir()?.join("jjq"))
}

/// Directory holding the flock files.
pub fn locks_dir() -> Result<PathBuf> {
    Ok(jjq_dir()?.join("locks"))
}

/// Path to the run log (last check command output).
pub fn run_log() -> Result<PathBuf> {
    Ok(jjq_dir()?.join("run.log"))
}
```

Rationale for a dedicated module over changing literals in place: the scope
explicitly includes future-proofing the layout. A single module gives new
filesystem state one obvious home and a naming convention, and keeps
`lock.rs` / `runlog.rs` free of path-assembly details.

### 2. Repoint call sites

- `src/lock.rs` — `lock_file_path(name)` becomes
  `Ok(paths::locks_dir()?.join(format!("{name}.lock")))`. The existing
  `fs::create_dir_all(path.parent().unwrap())` continues to create
  `.jj/jjq/locks/` on demand, so no separate directory-creation step is needed.
- `src/runlog.rs` — `log_path()` delegates to `paths::run_log()`.

No behavior change beyond the path: locks are still created on demand, the log
is still written by the runner and read by `tail`.

### 3. Legacy cleanup (no migration)

Old paths are abandoned rather than migrated — both are ephemeral (flock files
are recreated on demand; the log holds only the most recent check's output, so
nothing durable is lost). To avoid leaving cruft in already-initialized repos:

- **`clean`** — after sweeping workspaces, also remove `.jj/jjq-locks/` and
  `.jj/jjq-run.log` if present. `clean` already holds the `run` lock and probes
  `check` exclusively, so it will not remove a lock file out from under an
  active run or check. This is the one-shot cleanup path.
- **`doctor`** — report (do not delete) any legacy leftovers as an
  informational line that points the user at `jjq clean`. `doctor` stays
  diagnostic; `clean` performs the mutation. (`doctor --fix` continues to only
  repair the metadata bookmark; it does not touch these paths.)

Explicitly not done: no auto-migration on startup, and no cross-checking of the
legacy `run.lock` during an upgrade. This leaves a small window where an
old-version runner holding `.jj/jjq-locks/run.lock` and a new-version
invocation using `.jj/jjq/locks/run.lock` would not contend. This is acceptable
for a single-user tool where concurrent invocations mid-upgrade are rare and
the blast radius is one duplicated run attempt.

### 4. Docs

Update to the new paths:

- `docs/README.md` — lock and run-log path references
- `docs/jjq.1` — path references; regenerate `docs/jjq.1.txt` via
  `mandoc -Tascii -O width=78 docs/jjq.1 > docs/jjq.1.txt`
- `docs/jj-transaction-model.md` — lock-file location section
- `AGENTS.md` — Data Storage and Concurrency sections

Add a short "filesystem layout" note (in `docs/README.md` and `AGENTS.md`)
documenting `.jj/jjq/` as the home for jjq filesystem state and the convention
that new state goes there.

Historical `docs/plans/*` documents are left unchanged — they are dated design
records, not current documentation.

### 5. Tests

- Update the existing `.jj/jjq-locks` / `.jj/jjq-run.log` references in
  `tests/e2e.rs` (~6 sites: log-path assertions and lock-dir assertions) to the
  new `.jj/jjq/locks` / `.jj/jjq/run.log` paths.
- Add a test: with legacy `.jj/jjq-locks/` and `.jj/jjq-run.log` present,
  `jjq clean` removes them.
- Review and accept any insta snapshots that surface these paths.

## Surface area

- New file: `src/paths.rs` (+ `mod paths;` in `main.rs`)
- `src/lock.rs` — repoint `lock_file_path`
- `src/runlog.rs` — repoint `log_path`
- `src/commands.rs` — `clean` legacy removal, `doctor` legacy reporting
- Docs: `docs/README.md`, `docs/jjq.1` (+ `.txt`), `docs/jj-transaction-model.md`,
  `AGENTS.md`
- `tests/e2e.rs` — path updates + one new test

## Out of scope

- Migrating or preserving legacy file contents (they are ephemeral).
- Any change to the `jjq/*` bookmark namespace or the `jjq/_/_` metadata branch.
- Configurable path overrides / env vars for the jjq state directory.
