// ABOUTME: Filesystem-based locking using mkdir atomicity.
// ABOUTME: Implements the locking protocol from the jjq specification.

use anyhow::{bail, Result};
use std::fs;
use std::path::PathBuf;

use crate::jj;

/// A held lock that releases on drop.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Try to acquire a named lock.
    pub fn acquire(name: &str) -> Result<Option<Lock>> {
        let lock_dir = get_lock_dir()?;
        let lock_path = lock_dir.join(name);

        // Ensure parent directory exists
        fs::create_dir_all(&lock_dir)?;

        // Try to create the lock directory atomically
        match fs::create_dir(&lock_path) {
            Ok(()) => {
                // Write PID file
                let pid_path = lock_path.join("pid");
                fs::write(&pid_path, std::process::id().to_string())?;
                Ok(Some(Lock { path: lock_path }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Acquire a lock, failing with an error if already held.
    #[allow(dead_code)]
    pub fn acquire_or_fail(name: &str, message: &str) -> Result<Lock> {
        match Lock::acquire(name)? {
            Some(lock) => Ok(lock),
            None => bail!("{}", message),
        }
    }

    /// Release the lock explicitly.
    #[allow(dead_code)]
    pub fn release(self) {
        // Drop handles this
        drop(self);
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Check if a lock is currently held.
pub fn is_held(name: &str) -> Result<bool> {
    let lock_dir = get_lock_dir()?;
    let lock_path = lock_dir.join(name);
    Ok(lock_path.is_dir())
}

/// Get the lock directory path.
fn get_lock_dir() -> Result<PathBuf> {
    let root = jj::repo_root()?;
    Ok(root.join(".jj").join("jjq-locks"))
}
