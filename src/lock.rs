// ABOUTME: Filesystem-based locking using flock via the fs2 crate.
// ABOUTME: Implements the locking protocol from the jjq specification.

use anyhow::Result;
use fs2::FileExt;
use std::cell::RefCell;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crate::paths;

/// Position of each lock in the acquisition order. A blocking acquire may
/// only take a lock ranked after everything the thread already holds; this
/// keeps the documented orders (mutation -> publication -> meta for
/// producers, run -> publication -> meta for claims, run -> check -> meta
/// for clean) acyclic and is checked in debug builds. Non-blocking
/// try-acquires cannot deadlock and are exempt.
fn rank(name: &str) -> u8 {
    match name {
        "mutation" => 0,
        "run" => 1,
        "publication" => 2,
        "check" => 3,
        "meta" => 4,
        _ => u8::MAX,
    }
}

thread_local! {
    static HELD: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Debug-build check that a blocking acquire of `name` respects the lock
/// order. Callers that wait for a lock in their own loop (the metadata
/// writer's timed wait) call this before waiting.
pub fn assert_blocking_order(name: &str) {
    debug_assert!(
        HELD.with(|held| held.borrow().iter().all(|held| *held < rank(name))),
        "lock order violation: blocking on '{}' while holding a lock ranked after it",
        name
    );
}

/// How long a blocking acquire stays silent before telling the user what it
/// is waiting for. Brief contention is normal; a long wait without output
/// looks like a hang.
pub const SILENT_WAIT: Duration = Duration::from_secs(1);

/// The notice a blocking acquire prints once its wait exceeds `SILENT_WAIT`.
/// The metadata writer's timed wait prints the same line.
pub fn wait_notice(name: &str) {
    eprintln!(
        "jjq: waiting for another jjq process to release the {} lock",
        name
    );
}

/// A held lock that releases on drop (OS releases flock when File is dropped).
pub struct Lock {
    _file: File,
    rank: u8,
}

impl Lock {
    fn held(file: File, name: &str) -> Lock {
        let rank = rank(name);
        HELD.with(|held| held.borrow_mut().push(rank));
        Lock { _file: file, rank }
    }

    /// Acquire a named exclusive lock, waiting until its owner releases it.
    /// The operating system releases flock locks if the owning process exits.
    pub fn acquire_blocking(name: &str) -> Result<Lock> {
        assert_blocking_order(name);
        let path = lock_file_path(name)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let file = File::create(&path)?;
        let deadline = Instant::now() + SILENT_WAIT;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Lock::held(file, name)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        wait_notice(name);
        FileExt::lock_exclusive(&file)?;
        Ok(Lock::held(file, name))
    }

    /// Try to acquire a named lock. Returns Ok(Some) if acquired, Ok(None) if
    /// already held by another process.
    pub fn acquire(name: &str) -> Result<Option<Lock>> {
        let path = lock_file_path(name)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let file = File::create(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Lock::held(file, name))),
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
            Ok(()) => Ok(Some(Lock::held(file, name))),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(index) = held.iter().rposition(|rank| *rank == self.rank) {
                held.remove(index);
            }
        });
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
        // Only a contended lock means Held; other I/O errors (e.g. a
        // removed lock directory) must surface instead of masquerading as
        // "held by another process".
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(LockState::Held),
        Err(e) => Err(e.into()),
    }
}

/// Check if a lock is currently held.
pub fn is_held(name: &str) -> Result<bool> {
    Ok(matches!(lock_state(name)?, LockState::Held))
}

/// Get the lock file path for a named lock. Locks live in the shared .jj
/// directory so that invocations from any workspace contend on the same files.
fn lock_file_path(name: &str) -> Result<PathBuf> {
    Ok(paths::locks_dir()?.join(format!("{}.lock", name)))
}
