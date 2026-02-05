// ABOUTME: Configuration management for jjq stored on the metadata branch.
// ABOUTME: Handles reading/writing config values from config/ directory.

use anyhow::{Result, bail};
use std::convert::TryFrom;
use std::env;
use std::fs;
use std::io::IsTerminal;
use tempfile::TempDir;

use crate::jj;

/// The jjq metadata bookmark name.
pub const JJQ_BOOKMARK: &str = "jjq/_/_";

/// Default configuration values.
pub const DEFAULT_TRUNK_BOOKMARK: &str = "main";

/// Merge strategy for landing commits on trunk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
    Merge,
    Rebase,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Merge => "merge",
            Strategy::Rebase => "rebase",
        }
    }
}

impl TryFrom<&str> for Strategy {
    type Error = &'static str;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "merge" => Ok(Strategy::Merge),
            "rebase" => Ok(Strategy::Rebase),
            _ => Err("unknown strategy"),
        }
    }
}

/// Default strategy for existing repos (backward compat).
pub const DEFAULT_STRATEGY: Strategy = Strategy::Merge;

/// Valid configuration keys.
pub const VALID_KEYS: &[&str] = &["trunk_bookmark", "check_command", "strategy"];

/// Check if jjq is initialized (metadata bookmark exists).
pub fn is_initialized() -> Result<bool> {
    jj::bookmark_exists(JJQ_BOOKMARK)
}

/// Create the jjq metadata branch. Errors if already initialized.
pub fn initialize() -> Result<()> {
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

/// Ensure jjq is initialized, creating metadata branch if needed.
pub fn ensure_initialized() -> Result<()> {
    if is_initialized()? {
        return Ok(());
    }
    initialize()
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

/// Get the merge strategy.
pub fn get_strategy() -> Result<Strategy> {
    match get("strategy")? {
        Some(value) => Strategy::try_from(value.as_str())
            .map_err(|_| anyhow::anyhow!("invalid strategy value: {}", value)),
        None => Ok(DEFAULT_STRATEGY),
    }
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

    // Validate strategy values
    if key == "strategy" && Strategy::try_from(value).is_err() {
        bail!(
            "invalid value for strategy: {}\nvalid values: rebase, merge",
            value
        );
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

/// Show a one-time hint about configuring jj log filter.
/// Only shows if: stdout is a TTY, log filter not already configured, hint not shown before.
pub fn maybe_show_log_hint() -> Result<()> {
    // Skip if not a terminal (unless JJQTEST_FORCE_HINT is set for testing)
    let force_hint = env::var("JJQTEST_FORCE_HINT").is_ok();
    if !force_hint && !std::io::stdout().is_terminal() {
        return Ok(());
    }

    // Skip if log filter already configured to hide jjq metadata
    if let Ok(Some(current_log)) = jj::config_get("revsets.log")
        && current_log.contains(JJQ_BOOKMARK)
    {
        return Ok(());
    }

    // Skip if hint already shown (check metadata)
    if hint_already_shown()? {
        return Ok(());
    }

    // Show hint
    eprintln!();
    eprintln!("hint: To hide jjq metadata from 'jj log', run:");
    eprintln!("  jj config set --repo revsets.log '~ ::{}'", JJQ_BOOKMARK);
    eprintln!();

    // Record hint shown
    record_hint_shown()?;

    Ok(())
}

/// Check if the log hint has already been shown.
fn hint_already_shown() -> Result<bool> {
    if !is_initialized()? {
        return Ok(false);
    }
    match jj::file_show("log_hint_shown", JJQ_BOOKMARK) {
        Ok(_) => Ok(true),   // File exists = hint was shown
        Err(_) => Ok(false), // File doesn't exist = hint not shown
    }
}

/// Record that the log hint has been shown.
fn record_hint_shown() -> Result<()> {
    if !is_initialized()? {
        return Ok(());
    }

    let temp_dir = TempDir::new()?;
    let workspace_name = format!("jjq-hint-{}", std::process::id());

    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[JJQ_BOOKMARK],
    )?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(temp_dir.path())?;

    fs::write("log_hint_shown", "1")?;
    jj::describe("@", "record log hint shown")?;
    jj::run_quiet(&["bookmark", "set", JJQ_BOOKMARK])?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    Ok(())
}
