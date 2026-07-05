// ABOUTME: Utilities for the jjq run log file, including path resolution and sentinel markers.
// ABOUTME: The sentinel line marks the end of a check command's output in the log.

use anyhow::Result;
use std::path::PathBuf;

/// Prefix used to identify sentinel lines in log output.
pub const SENTINEL_PREFIX: &str = "--- jjq: run complete";

/// Build the sentinel line that marks the end of a run, including its exit code.
pub fn sentinel_line(exit_code: i32) -> String {
    format!("{} (exit {}) ---", SENTINEL_PREFIX, exit_code)
}

/// Return the path to the jjq run log file within the shared .jj directory,
/// so that a run started in one workspace is visible to `jjq tail` in another.
pub fn log_path() -> Result<PathBuf> {
    let dir = crate::jj::shared_jj_dir()?;
    Ok(dir.join("jjq-run.log"))
}
