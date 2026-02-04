// ABOUTME: Filesystem-based locking using flock via the fs2 crate.
// ABOUTME: Implements the locking protocol from the jjq specification.

use anyhow::{bail, Result};
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
            None => bail!("{}", message),
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

/// Get the lock file path for a named lock.
fn lock_file_path(name: &str) -> Result<PathBuf> {
    let root = jj::repo_root()?;
    Ok(root.join(".jj").join("jjq-locks").join(format!("{}.lock", name)))
}
