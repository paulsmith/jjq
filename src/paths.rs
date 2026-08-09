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

/// Path to the archived check output for a failed queue item.
pub fn failed_log(id: u32) -> Result<PathBuf> {
    Ok(jjq_dir()?
        .join("failed-logs")
        .join(format!("{:06}.log", id)))
}

/// Directory containing ephemeral, atomically-published active check state.
pub fn active_checks_dir() -> Result<PathBuf> {
    Ok(jjq_dir()?.join("active-checks"))
}

/// Durable cooperative cancellation intents, keyed by never-reused queue ID.
pub fn cancellations_dir() -> Result<PathBuf> {
    Ok(jjq_dir()?.join("cancellations"))
}
