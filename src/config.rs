// ABOUTME: Configuration management for jjq stored on the metadata branch.
// ABOUTME: Handles reading/writing config values from config/ directory.

use anyhow::{bail, Result};
use std::env;
use std::fs;
use tempfile::TempDir;

use crate::jj;

/// The jjq metadata bookmark name.
pub const JJQ_BOOKMARK: &str = "jjq/_/_";

/// Default configuration values.
pub const DEFAULT_TRUNK_BOOKMARK: &str = "main";
pub const DEFAULT_MAX_FAILURES: u32 = 3;

/// Valid configuration keys.
pub const VALID_KEYS: &[&str] = &["trunk_bookmark", "check_command", "max_failures"];

/// Check if jjq is initialized (metadata bookmark exists).
pub fn is_initialized() -> Result<bool> {
    jj::bookmark_exists(JJQ_BOOKMARK)
}

/// Ensure jjq is initialized, creating metadata branch if needed.
pub fn ensure_initialized() -> Result<()> {
    if is_initialized()? {
        return Ok(());
    }

    // Create new revision parented to root()
    let change_id = jj::new_rev(&["root()"])?;
    jj::run_quiet(&["bookmark", "create", "-r", &change_id, JJQ_BOOKMARK])?;

    // Create workspace to set up initial state
    let temp_dir = TempDir::new()?;
    let workspace_name = format!("jjq{}", std::process::id());

    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[JJQ_BOOKMARK],
    )?;

    // Change to workspace and create last_id file
    let orig_dir = env::current_dir()?;
    env::set_current_dir(temp_dir.path())?;

    fs::write("last_id", "0")?;
    jj::describe("@", "init jjq")?;
    jj::run_quiet(&["squash"])?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    Ok(())
}

/// Get a config value from the metadata branch.
pub fn get(key: &str) -> Result<Option<String>> {
    let path = format!("config/{}", key);
    match jj::file_show(&path, JJQ_BOOKMARK) {
        Ok(value) => Ok(Some(value.trim().to_string())),
        Err(_) => Ok(None),
    }
}

/// Get a config value with a default.
pub fn get_or_default(key: &str, default: &str) -> Result<String> {
    Ok(get(key)?.unwrap_or_else(|| default.to_string()))
}

/// Get the trunk bookmark name.
pub fn get_trunk_bookmark() -> Result<String> {
    get_or_default("trunk_bookmark", DEFAULT_TRUNK_BOOKMARK)
}

/// Get the check command (None if not configured).
pub fn get_check_command() -> Result<Option<String>> {
    get("check_command")
}

/// Get max failures to display.
pub fn get_max_failures() -> Result<u32> {
    let val = get_or_default("max_failures", &DEFAULT_MAX_FAILURES.to_string())?;
    Ok(val.parse().unwrap_or(DEFAULT_MAX_FAILURES))
}

/// Set a config value on the metadata branch.
pub fn set(key: &str, value: &str) -> Result<()> {
    // Validate key
    if !VALID_KEYS.contains(&key) {
        bail!(
            "unknown config key: {}\nvalid keys: {}",
            key,
            VALID_KEYS.join(", ")
        );
    }

    // Validate max_failures is numeric
    if key == "max_failures" {
        if value.parse::<u32>().is_err() {
            bail!("max_failures must be a non-negative integer");
        }
    }

    ensure_initialized()?;

    let temp_dir = TempDir::new()?;
    let workspace_name = format!("jjq-config-{}", std::process::id());

    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[JJQ_BOOKMARK],
    )?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(temp_dir.path())?;

    fs::create_dir_all("config")?;
    fs::write(format!("config/{}", key), value)?;
    jj::describe("@", &format!("config: set {}", key))?;
    jj::run_quiet(&["bookmark", "set", JJQ_BOOKMARK])?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    Ok(())
}
