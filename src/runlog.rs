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

/// Return the path to the jjq run log file within the repository's .jj directory.
pub fn log_path() -> Result<PathBuf> {
    let root = crate::jj::repo_root()?;
    Ok(root.join(".jj").join("jjq-run.log"))
}
