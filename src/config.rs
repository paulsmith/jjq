// ABOUTME: Configuration management for jjq stored on the metadata branch.
// ABOUTME: Handles reading/writing config values from config/ directory.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::sync::OnceLock;

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

/// Write the complete initial configuration in one metadata transaction. The
/// locked writer rejects a completed branch, permits recovery from an
/// interrupted bare metadata root, and prevents concurrent initializations
/// from both succeeding or leaving split config.
pub fn initialize(trunk_bookmark: &str, check_command: &str, strategy: Strategy) -> Result<()> {
    let description =
        "init jjq\nconfig: set trunk_bookmark\nconfig: set check_command\nconfig: set strategy";
    meta::with_initial_described_meta_workspace(description, |workspace| {
        let config_dir = workspace.join("config");
        fs::create_dir_all(&config_dir)?;
        fs::write(config_dir.join("trunk_bookmark"), trunk_bookmark)?;
        fs::write(config_dir.join("check_command"), check_command)?;
        fs::write(config_dir.join("strategy"), strategy.as_str())?;
        Ok(())
    })
}

/// A point-in-time view of the metadata branch, pinned to one commit so all
/// reads in a command see the same generation even when a writer lands
/// concurrently. Reads take no lock.
pub struct Snapshot {
    commit_id: String,
    config: OnceLock<BTreeMap<String, String>>,
}

/// Pin the current metadata commit. Returns None when jjq is not initialized;
/// errors when the metadata bookmark is conflicted (e.g. moved out-of-band).
pub fn snapshot() -> Result<Option<Snapshot>> {
    match jj::resolve_pinned_revset_full_state(&meta::meta_revset())? {
        jj::RevsetResolution::Absent => Ok(None),
        jj::RevsetResolution::Single(_change_id, commit_id) => Ok(Some(Snapshot {
            commit_id,
            config: OnceLock::new(),
        })),
        jj::RevsetResolution::Multiple => {
            bail!("jjq metadata bookmark is conflicted — run 'jjq doctor'")
        }
    }
}

impl Snapshot {
    /// Read a file from the pinned metadata commit. Ok(None) means the file
    /// does not exist; any other failure (unreadable repo state) is an error.
    pub fn file(&self, path: &str) -> Result<Option<String>> {
        match jj::file_show_pinned(path, &self.commit_id) {
            Ok(contents) => Ok(Some(contents)),
            Err(e) if e.to_string().contains("No such path") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read all files below a path from this exact metadata generation in one
    /// jj process. A missing path is a normal empty result.
    pub fn files_contents(&self, path: &str) -> Result<Vec<(String, String)>> {
        match jj::file_show_many_pinned(path, &self.commit_id) {
            Ok(files) => Ok(files),
            Err(e) if e.to_string().contains("No such path") => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    fn config_values(&self) -> Result<&BTreeMap<String, String>> {
        if let Some(values) = self.config.get() {
            return Ok(values);
        }
        let values = self
            .files_contents("config")?
            .into_iter()
            .filter_map(|(path, contents)| {
                let key = path.strip_prefix("config/")?;
                if key.contains('/') {
                    return None;
                }
                Some((key.to_string(), contents.trim().to_string()))
            })
            .collect();
        let _ = self.config.set(values);
        Ok(self
            .config
            .get()
            .expect("config cache was just initialized"))
    }

    /// Get the trunk bookmark name.
    pub fn trunk_bookmark(&self) -> Result<String> {
        Ok(self
            .config_values()?
            .get("trunk_bookmark")
            .cloned()
            .unwrap_or_else(|| DEFAULT_TRUNK_BOOKMARK.to_string()))
    }

    /// Get the check command (None if not configured).
    pub fn check_command(&self) -> Result<Option<String>> {
        Ok(self.config_values()?.get("check_command").cloned())
    }

    /// Get the merge strategy.
    pub fn strategy(&self) -> Result<Strategy> {
        match self.config_values()?.get("strategy") {
            Some(value) => Strategy::try_from(value.as_str())
                .map_err(|_| anyhow::anyhow!("invalid strategy value: {}", value)),
            None => Ok(DEFAULT_STRATEGY),
        }
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
    match snapshot()? {
        Some(snap) => snap.check_command(),
        None => Ok(None),
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

    let description = format!("config: set {}", key);
    meta::with_described_meta_workspace(&description, |workspace| {
        let config_dir = workspace.join("config");
        fs::create_dir_all(&config_dir)?;
        fs::write(config_dir.join(key), value)?;
        Ok(())
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
    eprintln!(
        "  jj config set --repo revsets.log '~ ::{}'",
        meta::meta_revset()
    );
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
    meta::with_described_meta_workspace("record log hint shown", |workspace| {
        fs::write(workspace.join("log_hint_shown"), "1")?;
        Ok(())
    })
}
