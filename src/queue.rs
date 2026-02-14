// ABOUTME: Queue management for jjq - sequence IDs and queue operations.
// ABOUTME: Handles bookmark-based queue state and FIFO ordering.

use anyhow::{bail, Result};
use regex::Regex;
use std::env;
use std::fs;
use tempfile::TempDir;

use crate::config::{self, JJQ_BOOKMARK};
use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::lock::Lock;

/// Validate and parse a sequence ID from user input.
/// Returns the integer value on success.
pub fn parse_seq_id(input: &str) -> Result<u32> {
    if input.is_empty() {
        bail!("invalid sequence ID: empty");
    }

    // Only digits allowed
    if !input.chars().all(|c| c.is_ascii_digit()) {
        bail!("invalid sequence ID: '{}' (must be numeric)", input);
    }

    let id: u32 = input
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid sequence ID: {} (must be 1-999999)", input))?;

    if !(1..=999999).contains(&id) {
        bail!("invalid sequence ID: {} (must be 1-999999)", input);
    }

    Ok(id)
}

/// Format a sequence ID as zero-padded string for bookmark names.
pub fn format_seq_id(id: u32) -> String {
    format!("{:06}", id)
}

/// Get the next sequence ID, incrementing the counter.
pub fn next_id() -> Result<u32> {
    let _lock = match Lock::acquire("id")? {
        Some(lock) => lock,
        None => {
            return Err(ExitError::new(
                exit_codes::LOCK_HELD,
                "could not acquire sequence ID lock (another process may be pushing)",
            )
            .into());
        }
    };

    config::ensure_initialized()?;

    let temp_dir = TempDir::new()?;
    let workspace_name = format!("jjq{}", std::process::id());

    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[JJQ_BOOKMARK],
    )?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(temp_dir.path())?;

    // Read current value
    let current: u32 = fs::read_to_string("last_id")?.trim().parse().unwrap_or(0);

    if current >= 999999 {
        env::set_current_dir(&orig_dir)?;
        jj::workspace_forget(&workspace_name)?;
        return Err(ExitError::new(exit_codes::USAGE, "sequence ID exhausted (at 999999)").into());
    }

    let new_id = current + 1;
    fs::write("last_id", new_id.to_string())?;
    jj::describe("@", &format!("{} -> {}\npid: {}", current, new_id, std::process::id()))?;
    jj::run_quiet(&["bookmark", "set", JJQ_BOOKMARK])?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    Ok(new_id)
}

/// Get all queued items sorted by sequence ID (ascending).
pub fn get_queue() -> Result<Vec<u32>> {
    let re = Regex::new(r"^jjq/queue/(\d{6})$").unwrap();
    let bookmarks = jj::bookmark_list_glob("jjq/queue/??????")?;

    let mut ids: Vec<u32> = bookmarks
        .iter()
        .filter_map(|b| {
            re.captures(b)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
        })
        .collect();

    ids.sort();
    Ok(ids)
}

/// Get all failed items sorted by sequence ID (descending, for display).
pub fn get_failed() -> Result<Vec<u32>> {
    let re = Regex::new(r"^jjq/failed/(\d{6})$").unwrap();
    let bookmarks = jj::bookmark_list_glob("jjq/failed/??????")?;

    let mut ids: Vec<u32> = bookmarks
        .iter()
        .filter_map(|b| {
            re.captures(b)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
        })
        .collect();

    ids.sort_by(|a, b| b.cmp(a)); // Descending
    Ok(ids)
}

/// Get the next item to process (lowest sequence ID).
pub fn next_item() -> Result<Option<u32>> {
    let queue = get_queue()?;
    Ok(queue.into_iter().next())
}

/// Get the queue bookmark name for an ID.
pub fn queue_bookmark(id: u32) -> String {
    format!("jjq/queue/{}", format_seq_id(id))
}

/// Get the failed bookmark name for an ID.
pub fn failed_bookmark(id: u32) -> String {
    format!("jjq/failed/{}", format_seq_id(id))
}

/// Check if a queue item exists.
pub fn queue_item_exists(id: u32) -> Result<bool> {
    jj::bookmark_exists(&queue_bookmark(id))
}

/// Check if a failed item exists.
pub fn failed_item_exists(id: u32) -> Result<bool> {
    jj::bookmark_exists(&failed_bookmark(id))
}
