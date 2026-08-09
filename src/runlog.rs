// ABOUTME: Utilities for the jjq run log file, including path resolution and sentinel markers.
// ABOUTME: The sentinel line marks the end of a check command's output in the log.

use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Prefix used to identify sentinel lines in log output.
pub const SENTINEL_PREFIX: &str = "--- jjq: run complete";

/// Build the sentinel line that marks the end of a run, including its exit code.
pub fn sentinel_line(exit_code: i32) -> String {
    format!("{} (exit {}) ---", SENTINEL_PREFIX, exit_code)
}

/// Return the path to the jjq run log file within the shared .jj directory,
/// so that a run started in one workspace is visible to `jjq tail` in another.
pub fn log_path() -> Result<PathBuf> {
    crate::paths::run_log()
}

/// Return the deterministic path for a failed queue item's archived log.
pub fn failed_log_path(id: u32) -> Result<PathBuf> {
    crate::paths::failed_log(id)
}

/// Per-attempt log for a speculative batch check. Concurrent checks must not
/// share `run.log`, which is deliberately truncated by each check runner.
pub fn attempt_log(run_token: &str, id: u32) -> Result<PathBuf> {
    Ok(crate::paths::jjq_dir()?
        .join("run-logs")
        .join(run_token)
        .join(format!("{:06}.log", id)))
}

/// Make one completed attempt the log shown by the existing `jjq tail`.
pub fn publish_attempt_log(source: &Path) -> Result<()> {
    let destination = log_path()?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating run log directory {}", parent.display()))?;
    }
    fs::copy(source, &destination)
        .with_context(|| format!("publishing check log {}", source.display()))?;
    Ok(())
}

/// Copy a completed run log into the failed-item archive.
pub fn archive_failed(id: u32, source: &Path) -> Result<()> {
    let destination = failed_log_path(id)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating failed log directory {}", parent.display()))?;
    }
    fs::copy(source, &destination)
        .with_context(|| format!("archiving failed check output to {}", destination.display()))?;
    Ok(())
}

/// Remove a failed queue item's archived log, if present.
pub fn remove_failed(id: u32) -> Result<()> {
    let path = failed_log_path(id)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing failed log {}", path.display())),
    }
}
