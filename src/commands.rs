// ABOUTME: Command implementations for jjq CLI.
// ABOUTME: Each function implements one jjq subcommand per the specification.

use anyhow::{bail, Result};
use std::env;
use std::process::Command;
use tempfile::TempDir;

use crate::config;
use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::lock::{self, Lock};
use crate::queue;

/// Output with jjq: prefix to stdout.
fn prefout(msg: &str) {
    println!("jjq: {}", msg);
}

/// Output with jjq: prefix to stderr.
fn preferr(msg: &str) {
    eprintln!("jjq: {}", msg);
}

/// Push a revision onto the merge queue.
pub fn push(revset: &str) -> Result<()> {
    // Resolve the revset to verify it exists and is unique
    let _change_id = jj::resolve_revset(revset)?;

    // Get trunk bookmark for conflict check
    let trunk_bookmark = config::get_trunk_bookmark()?;

    // Verify trunk bookmark exists
    if !jj::bookmark_exists(&trunk_bookmark)? {
        bail!("trunk bookmark '{}' not found", trunk_bookmark);
    }

    // Pre-flight conflict check
    let check_workspace = TempDir::new()?;
    let check_workspace_name = format!("jjq-check-{}", std::process::id());

    jj::workspace_add(
        check_workspace.path().to_str().unwrap(),
        &check_workspace_name,
        &[&trunk_bookmark, revset],
    )?;

    let has_conflicts = jj::has_conflicts(&format!("{}@", check_workspace_name))?;

    jj::workspace_forget(&check_workspace_name)?;

    if has_conflicts {
        preferr(&format!(
            "revision '{}' conflicts with {}",
            revset, trunk_bookmark
        ));
        preferr(&format!(
            "rebase onto {} and resolve conflicts before pushing",
            trunk_bookmark
        ));
        return Err(ExitError::new(exit_codes::CONFLICT, "revision conflicts with trunk").into());
    }

    config::ensure_initialized()?;

    let id = queue::next_id()?;
    let bookmark = queue::queue_bookmark(id);

    jj::bookmark_create(&bookmark, revset)?;

    prefout(&format!("revision '{}' queued at {}", revset, id));

    // Show one-time hint about configuring jj log
    config::maybe_show_log_hint()?;

    Ok(())
}

/// Process queue items.
pub fn run(all: bool) -> Result<()> {
    if all {
        run_all()
    } else {
        match run_one()? {
            RunResult::Success => Ok(()),
            RunResult::Empty => Ok(()),
            RunResult::Failure(code, msg) => Err(ExitError::new(code, msg).into()),
        }
    }
}

enum RunResult {
    Success,
    Empty,
    Failure(i32, String),
}

fn run_all() -> Result<()> {
    let mut merged_count = 0;

    loop {
        match run_one()? {
            RunResult::Success => {
                merged_count += 1;
            }
            RunResult::Empty => {
                if merged_count > 0 {
                    prefout(&format!("processed {} item(s)", merged_count));
                }
                return Ok(());
            }
            RunResult::Failure(code, msg) => {
                if merged_count > 0 {
                    prefout(&format!("processed {} item(s) before failure", merged_count));
                }
                return Err(ExitError::new(code, msg).into());
            }
        }
    }
}

fn run_one() -> Result<RunResult> {
    let id = match queue::next_item()? {
        Some(id) => id,
        None => {
            prefout("queue is empty");
            return Ok(RunResult::Empty);
        }
    };

    prefout(&format!("processing queue item {}", id));

    config::ensure_initialized()?;

    // Get config values
    let trunk_bookmark = config::get_trunk_bookmark()?;
    let check_command = match config::get_check_command()? {
        Some(cmd) => cmd,
        None => {
            preferr("check_command not configured (use 'jjq config check_command <cmd>')");
            return Ok(RunResult::Failure(
                exit_codes::USAGE,
                "check_command not configured".to_string(),
            ));
        }
    };

    // Acquire run lock
    let run_lock = match Lock::acquire("run")? {
        Some(lock) => lock,
        None => {
            preferr("queue runner lock already held");
            return Ok(RunResult::Failure(exit_codes::LOCK_HELD, "run lock unavailable".to_string()));
        }
    };

    // Record trunk commit ID
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;

    // Create workspace for merge
    let runner_workspace = TempDir::new()?;
    let run_name = format!("jjq-run-{}", queue::format_seq_id(id));
    let queue_bookmark = queue::queue_bookmark(id);

    jj::workspace_add(
        runner_workspace.path().to_str().unwrap(),
        &run_name,
        &[
            &format!("bookmarks(exact:{})", trunk_bookmark),
            &format!("bookmarks(exact:{})", queue_bookmark),
        ],
    )?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(runner_workspace.path())?;

    // Check for conflicts
    let workspace_rev = format!("{}@", run_name);
    if jj::has_conflicts(&workspace_rev)? {
        jj::bookmark_delete(&queue_bookmark)?;
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::describe(&workspace_rev, &format!("Failed: merge {} (conflicts)", id))?;

        env::set_current_dir(&orig_dir)?;
        // Keep workspace for debugging
        let ws_path = runner_workspace.keep();
        drop(run_lock);

        preferr(&format!("merge {} has conflicts, marked as failed", id));
        preferr(&format!("workspace remains: {}", ws_path.display()));
        return Ok(RunResult::Failure(exit_codes::CONFLICT, format!("merge {} has conflicts", id)));
    }

    jj::describe(&workspace_rev, &format!("WIP: attempting merge {}", id))?;

    // Run check command
    let check_output = Command::new("sh")
        .arg("-c")
        .arg(&check_command)
        .output()?;

    if !check_output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&check_output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&check_output.stderr));

        jj::bookmark_delete(&queue_bookmark)?;
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::describe(&workspace_rev, &format!("Failed: merge {} (check)", id))?;

        env::set_current_dir(&orig_dir)?;
        let ws_path = runner_workspace.keep();
        drop(run_lock);

        preferr(&format!("merge {} check failed", id));
        preferr(&format!("workspace remains: {}", ws_path.display()));
        return Ok(RunResult::Failure(exit_codes::CHECK_FAILED, format!("merge {} check failed", id)));
    }

    // Verify trunk hasn't moved
    let current_trunk_commit_id =
        jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;
    if trunk_commit_id != current_trunk_commit_id {
        env::set_current_dir(&orig_dir)?;
        jj::workspace_forget(&run_name)?;
        drop(run_lock);

        preferr("trunk bookmark moved during run; queue item left in place, re-run to retry");
        return Ok(RunResult::Failure(exit_codes::TRUNK_MOVED, "trunk moved during run".to_string()));
    }

    // Success path
    let merge_change_id = jj::resolve_revset("@")?;

    jj::bookmark_delete(&queue_bookmark)?;
    jj::describe("@", &format!("Success: merge {}", id))?;
    jj::bookmark_move(&trunk_bookmark)?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&run_name)?;
    drop(run_lock);

    prefout(&format!(
        "merged {} to {} (now at {})",
        id, trunk_bookmark, merge_change_id
    ));
    Ok(RunResult::Success)
}

/// Display queue status.
pub fn status() -> Result<()> {
    if !config::is_initialized()? {
        prefout("jjq not initialized (run 'jjq push <revset>' to start)");
        return Ok(());
    }

    let max_failures = config::get_max_failures()?;

    // Check for active run
    if lock::is_held("run")? {
        prefout("Run in progress");
        println!();
    }

    let queue_items = queue::get_queue()?;
    let failed_items = queue::get_failed()?;

    if queue_items.is_empty() && failed_items.is_empty() {
        prefout("queue is empty");
        return Ok(());
    }

    if !queue_items.is_empty() {
        prefout("Queued:");
        for id in &queue_items {
            let bookmark = queue::queue_bookmark(*id);
            let info = jj::get_log_info(&format!("bookmarks(exact:{})", bookmark))?;
            println!("  {}: {}", id, info);
        }
    }

    if !failed_items.is_empty() {
        if !queue_items.is_empty() {
            println!();
        }
        prefout("Failed (recent):");
        for id in failed_items.iter().take(max_failures as usize) {
            let bookmark = queue::failed_bookmark(*id);
            let info = jj::get_log_info(&format!("bookmarks(exact:{})", bookmark))?;
            println!("  {}: {}", id, info);
        }
    }

    Ok(())
}

/// Retry a failed merge.
pub fn retry(id_str: &str, revset: Option<&str>) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;
    let failed_bookmark = queue::failed_bookmark(id);

    if !queue::failed_item_exists(id)? {
        bail!("failed item {} not found", id);
    }

    let candidate_revset = match revset {
        Some(r) => {
            // Verify revset exists
            jj::resolve_revset(r)?;
            prefout(&format!("retrying failed item {} using '{}'", id, r));
            r.to_string()
        }
        None => {
            // Find original candidate from failed merge parents
            let trunk_bookmark = config::get_trunk_bookmark()?;
            let original_candidate =
                jj::get_candidate_parent(&format!("bookmarks(exact:{})", failed_bookmark), &trunk_bookmark)?;
            prefout(&format!(
                "retrying failed item {} using original candidate {}",
                id, original_candidate
            ));
            original_candidate
        }
    };

    config::ensure_initialized()?;

    let new_id = queue::next_id()?;
    let new_bookmark = queue::queue_bookmark(new_id);

    jj::bookmark_create(&new_bookmark, &candidate_revset)?;
    jj::bookmark_delete(&failed_bookmark)?;

    prefout(&format!("revision queued at {}", new_id));
    Ok(())
}

/// Delete an item from queue or failed list.
pub fn delete(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;

    config::ensure_initialized()?;

    // Check queue first
    if queue::queue_item_exists(id)? {
        jj::bookmark_delete(&queue::queue_bookmark(id))?;
        prefout(&format!("deleted queued item {}", id));
        return Ok(());
    }

    // Check failed
    if queue::failed_item_exists(id)? {
        jj::bookmark_delete(&queue::failed_bookmark(id))?;
        prefout(&format!("deleted failed item {}", id));
        return Ok(());
    }

    bail!("item {} not found in queue or failed", id)
}

/// Get or set configuration.
pub fn config(key: Option<&str>, value: Option<&str>) -> Result<()> {
    match (key, value) {
        (None, None) => {
            // Show all config
            config::ensure_initialized()?;
            let trunk = config::get_trunk_bookmark()?;
            let check = config::get_check_command()?;
            let max_fail = config::get_max_failures()?;

            println!("trunk_bookmark = {}", trunk);
            println!(
                "check_command = {}",
                check.unwrap_or_else(|| "(not set)".to_string())
            );
            println!("max_failures = {}", max_fail);
            Ok(())
        }
        (Some(k), None) => {
            // Get single value
            if !config::VALID_KEYS.contains(&k) {
                bail!(
                    "unknown config key: {}\nvalid keys: {}",
                    k,
                    config::VALID_KEYS.join(", ")
                );
            }

            let value = match k {
                "trunk_bookmark" => config::get_trunk_bookmark()?,
                "check_command" => config::get_check_command()?.unwrap_or_default(),
                "max_failures" => config::get_max_failures()?.to_string(),
                _ => unreachable!(),
            };
            println!("{}", value);
            Ok(())
        }
        (Some(k), Some(v)) => {
            // Set value
            config::set(k, v)?;
            prefout(&format!("{} = {}", k, v));
            Ok(())
        }
        (None, Some(_)) => {
            bail!("cannot set value without key")
        }
    }
}
