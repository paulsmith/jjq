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

    /// Try to acquire a named lock in shared mode. Any number of shared
    /// holders may coexist (e.g. concurrent `jjq check` runs), while an
    /// exclusive `acquire` of the same name fails as long as one lives —
    /// which is how `jjq clean` detects active checks.
    pub fn acquire_shared(name: &str) -> Result<Option<Lock>> {
        let path = lock_file_path(name)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let file = File::create(&path)?;
        // Qualified call: std::fs::File has grown an inherent
        // try_lock_shared with a different error type, which would shadow
        // the fs2 trait method the rest of this module uses.
        match FileExt::try_lock_shared(&file) {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
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

/// Get the lock file path for a named lock. Locks live in the shared .jj
/// directory so that invocations from any workspace contend on the same files.
fn lock_file_path(name: &str) -> Result<PathBuf> {
    let dir = jj::shared_jj_dir()?;
    Ok(dir.join("jjq-locks").join(format!("{}.lock", name)))
}
