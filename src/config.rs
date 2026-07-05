// ABOUTME: Configuration management for jjq stored on the metadata branch.
// ABOUTME: Handles reading/writing config values from config/ directory.

use anyhow::{Result, bail};
use std::env;
use std::fs;
use std::io::IsTerminal;

use crate::jj;
use crate::meta::{self, JJQ_BOOKMARK};

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

/// Create the jjq metadata branch. The branch itself (with its last_id seed)
/// is created inside the metadata writer, under the meta lock, so concurrent
/// initializations serialize instead of racing the bookmark. Any metadata
/// write auto-creates the branch the same way; this exists so `jjq init` can
/// record an explicit "init jjq" entry in the metadata operation log.
pub fn initialize() -> Result<()> {
    meta::with_meta_workspace(|_workspace| Ok("init jjq".to_string()))
}

/// A point-in-time view of the metadata branch, pinned to one commit so all
/// reads in a command see the same generation even when a writer lands
/// concurrently. Reads take no lock.
pub struct Snapshot {
    commit_id: String,
}

/// Pin the current metadata commit. Returns None when jjq is not initialized;
/// errors when the metadata bookmark is conflicted (e.g. moved out-of-band).
pub fn snapshot() -> Result<Option<Snapshot>> {
    match jj::resolve_revset_full(&meta::meta_revset()) {
        Ok((_change_id, commit_id)) => Ok(Some(Snapshot { commit_id })),
        Err(e) if e.to_string().contains("multiple revisions") => {
            bail!("jjq metadata bookmark is conflicted — run 'jjq doctor'")
        }
        Err(e) if e.to_string().contains("not found") => Ok(None),
        Err(e) => Err(e),
    }
}

impl Snapshot {
    /// Read a file from the pinned metadata commit. Ok(None) means the file
    /// does not exist; any other failure (unreadable repo state) is an error.
    pub fn file(&self, path: &str) -> Result<Option<String>> {
        match jj::file_show(path, &self.commit_id) {
            Ok(contents) => Ok(Some(contents)),
            Err(e) if e.to_string().contains("No such path") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read a config key from the pinned metadata commit.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .file(&format!("config/{}", key))?
            .map(|v| v.trim().to_string()))
    }

    /// Get the trunk bookmark name.
    pub fn trunk_bookmark(&self) -> Result<String> {
        Ok(self
            .get("trunk_bookmark")?
            .unwrap_or_else(|| DEFAULT_TRUNK_BOOKMARK.to_string()))
    }

    /// Get the check command (None if not configured).
    pub fn check_command(&self) -> Result<Option<String>> {
        self.get("check_command")
    }

    /// Get the merge strategy.
    pub fn strategy(&self) -> Result<Strategy> {
        match self.get("strategy")? {
            Some(value) => Strategy::try_from(value.as_str())
                .map_err(|_| anyhow::anyhow!("invalid strategy value: {}", value)),
            None => Ok(DEFAULT_STRATEGY),
        }
    }
}

/// Get a config value, or None when unset or jjq is uninitialized.
pub fn get(key: &str) -> Result<Option<String>> {
    match snapshot()? {
        Some(snap) => snap.get(key),
        None => Ok(None),
    }
}

/// Get the trunk bookmark name.
pub fn get_trunk_bookmark() -> Result<String> {
    match snapshot()? {
        Some(snap) => snap.trunk_bookmark(),
        None => Ok(DEFAULT_TRUNK_BOOKMARK.to_string()),
    }
}

/// Get the check command (None if not configured).
pub fn get_check_command() -> Result<Option<String>> {
    get("check_command")
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

    meta::with_meta_workspace(|workspace| {
        let config_dir = workspace.join("config");
        fs::create_dir_all(&config_dir)?;
        fs::write(config_dir.join(key), value)?;
        Ok(format!("config: set {}", key))
    })
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

    // Show hint. The bookmarks(exact:) form tolerates a conflicted bookmark;
    // a bare symbol would break jj's short-prefixes index if the metadata
    // bookmark ever became conflicted.
    eprintln!();
    eprintln!("hint: To hide jjq metadata from 'jj log', run:");
    eprintln!("  jj config set --repo revsets.log '~ ::{}'", meta::meta_revset());
    eprintln!();

    // Record hint shown
    record_hint_shown()?;

    Ok(())
}

/// Check if the log hint has already been shown.
fn hint_already_shown() -> Result<bool> {
    match snapshot()? {
        Some(snap) => Ok(snap.file("log_hint_shown")?.is_some()),
        None => Ok(false),
    }
}

/// Record that the log hint has been shown.
fn record_hint_shown() -> Result<()> {
    if !is_initialized()? {
        return Ok(());
    }
    meta::with_meta_workspace(|workspace| {
        fs::write(workspace.join("log_hint_shown"), "1")?;
        Ok("record log hint shown".to_string())
    })
}
