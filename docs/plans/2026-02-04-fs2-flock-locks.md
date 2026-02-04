# Replace mkdir Locks with fs2 File Locks

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the mkdir-based locking mechanism with fs2 flock-based file locks for simplicity and automatic stale lock cleanup.

**Architecture:** Lock files (`.jj/jjq-locks/{name}.lock`) replace lock directories. The `Lock` struct holds a `std::fs::File` handle; the OS releases the flock automatically when the handle is dropped or the process exits. `LockState` collapses from four variants to two (`Free`/`Held`) since PID-based diagnostics are removed.

**Tech Stack:** Rust, fs2 crate

---

### Task 1: Add fs2 dependency

**Files:**
- Modify: `Cargo.toml:10-14`

**Step 1: Add fs2 to dependencies**

In `Cargo.toml`, add `fs2` to `[dependencies]`:

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
anyhow = "1"
tempfile = "3"
regex = "1"
fs2 = "0.4"
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors

**Step 3: Commit**

```
jj commit -m "Add fs2 dependency"
```

---

### Task 2: Rewrite lock.rs to use fs2

**Files:**
- Modify: `src/lock.rs` (full rewrite of file)

**Step 1: Write the new lock.rs**

Replace the entire contents of `src/lock.rs` with:

```rust
// ABOUTME: Filesystem-based locking using flock via the fs2 crate.
// ABOUTME: Implements the locking protocol from the jjq specification.

use anyhow::Result;
use fs2::FileExt;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use crate::jj;

/// A held lock that releases on drop (OS releases flock when File is dropped).
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Try to acquire a named lock. Returns Ok(Some) if acquired, Ok(None) if
    /// already held by another process.
    pub fn acquire(name: &str) -> Result<Option<Lock>> {
        let path = lock_file_path(name)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let file = File::create(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Acquire a lock, failing with an error if already held.
    pub fn acquire_or_fail(name: &str, message: &str) -> Result<Lock> {
        match Lock::acquire(name)? {
            Some(lock) => Ok(lock),
            None => anyhow::bail!("{}", message),
        }
    }

    /// Release the lock explicitly.
    #[allow(dead_code)]
    pub fn release(self) {
        drop(self);
    }
}

/// State of a named lock.
pub enum LockState {
    Free,
    Held,
}

/// Inspect the state of a named lock.
pub fn lock_state(name: &str) -> Result<LockState> {
    let path = lock_file_path(name)?;
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LockState::Free),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()?;
            Ok(LockState::Free)
        }
        Err(_) => Ok(LockState::Held),
    }
}

/// Check if a lock is currently held.
pub fn is_held(name: &str) -> Result<bool> {
    matches!(lock_state(name)?, LockState::Held).pipe_ok()
}

/// Get the filesystem path for a named lock (for diagnostic messages).
pub fn lock_path(name: &str) -> Result<PathBuf> {
    lock_file_path(name)
}

/// Get the lock file path for a named lock.
fn lock_file_path(name: &str) -> Result<PathBuf> {
    let root = jj::repo_root()?;
    Ok(root.join(".jj").join("jjq-locks").join(format!("{}.lock", name)))
}
```

Wait — the `is_held` function above uses a non-existent method. Simplify it:

```rust
/// Check if a lock is currently held.
pub fn is_held(name: &str) -> Result<bool> {
    Ok(matches!(lock_state(name)?, LockState::Held))
}
```

Here is the complete, correct `src/lock.rs`:

```rust
// ABOUTME: Filesystem-based locking using flock via the fs2 crate.
// ABOUTME: Implements the locking protocol from the jjq specification.

use anyhow::Result;
use fs2::FileExt;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use crate::jj;

/// A held lock that releases on drop (OS releases flock when File is dropped).
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Try to acquire a named lock. Returns Ok(Some) if acquired, Ok(None) if
    /// already held by another process.
    pub fn acquire(name: &str) -> Result<Option<Lock>> {
        let path = lock_file_path(name)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let file = File::create(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Acquire a lock, failing with an error if already held.
    pub fn acquire_or_fail(name: &str, message: &str) -> Result<Lock> {
        match Lock::acquire(name)? {
            Some(lock) => Ok(lock),
            None => anyhow::bail!("{}", message),
        }
    }

    /// Release the lock explicitly.
    #[allow(dead_code)]
    pub fn release(self) {
        drop(self);
    }
}

/// State of a named lock.
pub enum LockState {
    Free,
    Held,
}

/// Inspect the state of a named lock.
pub fn lock_state(name: &str) -> Result<LockState> {
    let path = lock_file_path(name)?;
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LockState::Free),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()?;
            Ok(LockState::Free)
        }
        Err(_) => Ok(LockState::Held),
    }
}

/// Check if a lock is currently held.
pub fn is_held(name: &str) -> Result<bool> {
    Ok(matches!(lock_state(name)?, LockState::Held))
}

/// Get the filesystem path for a named lock (for diagnostic messages).
pub fn lock_path(name: &str) -> Result<PathBuf> {
    lock_file_path(name)
}

/// Get the lock file path for a named lock.
fn lock_file_path(name: &str) -> Result<PathBuf> {
    let root = jj::repo_root()?;
    Ok(root.join(".jj").join("jjq-locks").join(format!("{}.lock", name)))
}
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compile errors in `commands.rs` (doctor match arms reference removed variants). That's expected — we fix those in Task 3.

**Step 3: Commit**

```
jj commit -m "Rewrite lock.rs to use fs2 flock"
```

---

### Task 3: Update doctor command for simplified LockState

**Files:**
- Modify: `src/commands.rs:596-634`

**Step 1: Replace the doctor lock checks**

Replace lines 596-634 (the run lock and id lock match blocks) with:

```rust
    // 5. run lock
    match lock::lock_state("run")? {
        lock::LockState::Free => print_check("ok", "run lock is free"),
        lock::LockState::Held => {
            print_check("WARN", "run lock held by another process");
            warns += 1;
        }
    }

    // 6. id lock
    match lock::lock_state("id")? {
        lock::LockState::Free => print_check("ok", "id lock is free"),
        lock::LockState::Held => {
            print_check("WARN", "id lock held by another process");
            warns += 1;
        }
    }
```

Note: stale locks held by dead processes are no longer possible (the OS releases flock on process exit), so there is no FAIL case for locks anymore — only WARN for legitimately held locks.

**Step 2: Remove the `use std::process::Command` import if now unused**

Check if `std::process::Command` is still used elsewhere in `lock.rs`. It was only used for `kill -0`. It's already removed in the Task 2 rewrite — no action needed here.

**Step 3: Verify it compiles and tests pass**

Run: `cargo check && cargo test`
Expected: compiles, all tests pass

**Step 4: Commit**

```
jj commit -m "Simplify doctor lock checks for flock"
```

---

### Task 4: Update snapshot tests

**Files:**
- Modify: any snapshot files under `tests/` that contain doctor lock output

**Step 1: Check for affected snapshots**

Run: `cargo test`

If any snapshot tests fail because doctor output changed (e.g., "run lock is free" text is the same, but if any test exercises stale-lock scenarios), update the snapshots.

Run: `cargo insta review` (if insta snapshots are used)

**Step 2: Commit**

```
jj commit -m "Update snapshots for flock lock changes"
```

---

### Task 5: Clean up old lock directories

**Files:**
- None (manual/runtime concern)

**Step 1: Verify old `.jj/jjq-locks/id/` and `.jj/jjq-locks/run/` directories are not referenced anywhere**

Run: `grep -r 'jjq-locks' src/` and confirm only `.lock` file references remain.

**Step 2: Commit (if any remaining references were cleaned up)**

```
jj commit -m "Remove references to old lock directory layout"
```

---

### Task 6: Update specification docs

**Files:**
- Modify: `docs/specification.md` (if it documents the mkdir locking protocol)

**Step 1: Search for mkdir lock references in docs**

Run: `grep -rn 'mkdir\|lock.*dir\|pid.*file\|kill.*-0' docs/`

**Step 2: Update any found references to describe flock-based locking instead**

Replace mentions of:
- `mkdir` atomicity → flock-based file locking via fs2
- PID files → removed (OS handles cleanup)
- `kill -0` stale detection → removed (flock released on process exit)
- Lock directories → lock files (`.lock` extension)

**Step 3: Commit**

```
jj commit -m "Update docs for flock-based locking"
```
