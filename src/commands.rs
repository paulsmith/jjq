// ABOUTME: Command implementations for jjq CLI.
// ABOUTME: Each function implements one jjq subcommand per the specification.

use anyhow::{bail, Result};
use std::env;
use std::fs;
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

/// Extract the numeric ID from a bookmark name like "jjq/queue/000042".
fn extract_id_from_bookmark(bookmark: &str) -> u32 {
    bookmark
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
}

/// Extract the change ID from a "jjq-candidate: <id>" trailer in a description.
fn extract_candidate_trailer(description: &str) -> Option<String> {
    for line in description.lines() {
        if let Some(id) = line.strip_prefix("jjq-candidate: ") {
            return Some(id.trim().to_string());
        }
    }
    None
}

/// Look up the filesystem path of a workspace from jjq metadata log history.
fn lookup_workspace_path(id: u32) -> Option<String> {
    // Try reading workspace path from metadata file first
    let padded = queue::format_seq_id(id);
    let file_path = format!("workspace/{}", padded);
    if let Ok(path) = jj::file_show(&file_path, config::JJQ_BOOKMARK) {
        let path = path.trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }

    // Fall back to searching commit descriptions
    let output = jj::run_ok(&[
        "log",
        "-r",
        "ancestors(bookmarks(exact:\"jjq/_/_\"), 100)",
        "--no-graph",
        "-T",
        "description ++ \"\\n---\\n\"",
    ]);
    match output {
        Ok(text) => {
            let needle = format!("Sequence-Id: {}", id);
            for block in text.split("\n---\n") {
                if block.contains(&needle) {
                    for line in block.lines() {
                        if let Some(path) = line.strip_prefix("Workspace: ") {
                            return Some(path.trim().to_string());
                        }
                    }
                }
            }
            None
        }
        Err(_) => None,
    }
}

/// Record workspace path in metadata for later recovery by delete/clean.
fn record_workspace_metadata(id: u32, workspace_path: &str) -> Result<()> {
    let temp_dir = tempfile::TempDir::new()?;
    let workspace_name = format!("jjq-meta-{}", std::process::id());

    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[config::JJQ_BOOKMARK],
    )?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(temp_dir.path())?;

    fs::create_dir_all("workspace")?;
    fs::write(
        format!("workspace/{}", queue::format_seq_id(id)),
        workspace_path,
    )?;
    jj::describe(
        "@",
        &format!("Sequence-Id: {}\nWorkspace: {}", id, workspace_path),
    )?;
    jj::run_quiet(&["bookmark", "set", config::JJQ_BOOKMARK])?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    Ok(())
}

/// Push a revision onto the merge queue.
pub fn push(revset: &str) -> Result<()> {
    // Resolve both change ID and commit ID
    let (change_id, commit_id) = jj::resolve_revset_full(revset)
        .map_err(|e| ExitError::new(exit_codes::USAGE, e.to_string()))?;

    // Get trunk bookmark
    let trunk_bookmark = config::get_trunk_bookmark()?;

    // Verify trunk bookmark exists
    if !jj::bookmark_exists(&trunk_bookmark)? {
        return Err(ExitError::new(exit_codes::USAGE, format!("trunk bookmark '{}' not found", trunk_bookmark)).into());
    }

    // Idempotent push: clean up existing queue/failed entries for this change

    // Scan queue bookmarks
    let queue_bookmarks = jj::bookmark_list_glob("jjq/queue/??????")?;
    for bookmark in &queue_bookmarks {
        let entry_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", bookmark))?;
        if entry_commit_id == commit_id {
            let entry_id = extract_id_from_bookmark(bookmark);
            preferr(&format!("revision already queued at {}", entry_id));
            return Err(ExitError::new(exit_codes::USAGE, "revision already queued").into());
        }
        let entry_change_id = jj::resolve_revset(&format!("bookmarks(exact:{})", bookmark))?;
        if entry_change_id == change_id {
            let entry_id = extract_id_from_bookmark(bookmark);
            jj::bookmark_delete(bookmark)?;
            prefout(&format!("replacing queued entry {}", entry_id));
        }
    }

    // Scan failed bookmarks: extract candidate change ID from jjq-candidate trailer
    let failed_bookmarks = jj::bookmark_list_glob("jjq/failed/??????")?;
    for bookmark in &failed_bookmarks {
        let desc = jj::get_description(&format!("bookmarks(exact:{})", bookmark))?;
        if let Some(candidate_change_id) = extract_candidate_trailer(&desc)
            && candidate_change_id == change_id
        {
            let entry_id = extract_id_from_bookmark(bookmark);
            jj::bookmark_delete(bookmark)?;
            prefout(&format!("clearing failed entry {}", entry_id));
        }
    }

    // Pre-flight conflict check using headless merge commit
    let conflict_check_id = jj::new_rev(&[&trunk_bookmark, revset])?;
    let has_conflicts = jj::has_conflicts(&conflict_check_id)?;
    jj::abandon(&conflict_check_id)?;

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
pub fn run(all: bool, stop_on_failure: bool) -> Result<()> {
    if all {
        run_all(stop_on_failure)
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

fn run_all(stop_on_failure: bool) -> Result<()> {
    let mut merged_count = 0u32;
    let mut failed_count = 0u32;

    loop {
        match run_one()? {
            RunResult::Success => {
                merged_count += 1;
            }
            RunResult::Empty => {
                break;
            }
            RunResult::Failure(_code, msg) => {
                if stop_on_failure {
                    if merged_count > 0 {
                        prefout(&format!("processed {} item(s) before failure", merged_count));
                    }
                    return Err(ExitError::new(exit_codes::CONFLICT, msg).into());
                }
                failed_count += 1;
            }
        }
    }

    if merged_count > 0 || failed_count > 0 {
        if failed_count > 0 {
            prefout(&format!("processed {} item(s), {} failed", merged_count, failed_count));
            return Err(ExitError::new(
                exit_codes::PARTIAL,
                format!("processed {} item(s), {} failed", merged_count, failed_count),
            ).into());
        }
        prefout(&format!("processed {} item(s)", merged_count));
    }
    Ok(())
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

    // Acquire config lock to read settings
    let config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
    let trunk_bookmark = config::get_trunk_bookmark()?;
    let check_command = match config::get_check_command()? {
        Some(cmd) => cmd,
        None => {
            drop(config_lock);
            preferr("check_command not configured (use 'jjq config check_command <cmd>')");
            return Ok(RunResult::Failure(
                exit_codes::CONFLICT,
                "check_command not configured".to_string(),
            ));
        }
    };
    drop(config_lock);

    // Acquire run lock
    let run_lock = match Lock::acquire("run")? {
        Some(lock) => lock,
        None => {
            preferr("queue runner lock already held");
            return Ok(RunResult::Failure(exit_codes::CONFLICT, "run lock unavailable".to_string()));
        }
    };

    // Record trunk commit ID
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;

    // Capture candidate change ID before creating workspace
    let queue_bookmark = queue::queue_bookmark(id);
    let candidate_change_id = jj::resolve_revset(&format!("bookmarks(exact:{})", queue_bookmark))?;

    // Create workspace for merge
    let runner_workspace = TempDir::new()?;
    let run_name = format!("jjq-run-{}", queue::format_seq_id(id));

    jj::workspace_add(
        runner_workspace.path().to_str().unwrap(),
        &run_name,
        &[
            &format!("bookmarks(exact:{})", trunk_bookmark),
            &format!("bookmarks(exact:{})", queue_bookmark),
        ],
    )?;

    // Record the workspace path in metadata for later recovery by delete/clean
    record_workspace_metadata(id, runner_workspace.path().to_str().unwrap())?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(runner_workspace.path())?;

    // Check for conflicts
    let workspace_rev = format!("{}@", run_name);
    if jj::has_conflicts(&workspace_rev)? {
        jj::bookmark_delete(&queue_bookmark)?;
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::describe(
            &workspace_rev,
            &format!("Failed: merge {} (conflicts)\n\njjq-candidate: {}", id, candidate_change_id),
        )?;

        env::set_current_dir(&orig_dir)?;
        let ws_path = runner_workspace.keep();
        drop(run_lock);

        preferr(&format!("merge {} has conflicts, marked as failed", id));
        preferr(&format!("workspace: {}", ws_path.display()));
        preferr("");
        preferr("To resolve:");
        preferr(&format!("  1. Rebase your revision onto {} and resolve conflicts", trunk_bookmark));
        preferr("  2. Run: jjq push <fixed-revset>");
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
        jj::describe(
            &workspace_rev,
            &format!("Failed: merge {} (check)\n\njjq-candidate: {}", id, candidate_change_id),
        )?;

        env::set_current_dir(&orig_dir)?;
        let ws_path = runner_workspace.keep();
        drop(run_lock);

        preferr(&format!("merge {} failed check, marked as failed", id));
        preferr(&format!("workspace: {}", ws_path.display()));
        preferr("");
        preferr("To resolve:");
        preferr("  1. Fix the issue and create a new revision");
        preferr("  2. Run: jjq push <fixed-revset>");
        return Ok(RunResult::Failure(exit_codes::CONFLICT, format!("merge {} check failed", id)));
    }

    // Verify trunk hasn't moved
    let current_trunk_commit_id =
        jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;
    if trunk_commit_id != current_trunk_commit_id {
        env::set_current_dir(&orig_dir)?;
        jj::workspace_forget(&run_name)?;
        drop(run_lock);

        preferr("trunk bookmark moved during run; queue item left in place, re-run to retry");
        return Ok(RunResult::Failure(exit_codes::CONFLICT, "trunk moved during run".to_string()));
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

    // Acquire config lock to read settings
    let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
    let max_failures = config::get_max_failures()?;
    drop(_config_lock);

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
        let padded = queue::format_seq_id(id);
        let run_name = format!("jjq-run-{}", padded);

        // Look up workspace path before deleting
        let workspace_path = lookup_workspace_path(id);

        jj::bookmark_delete(&queue::failed_bookmark(id))?;
        prefout(&format!("deleted failed item {}", id));

        // Try to forget the workspace (silently ignore if not found)
        let _ = jj::workspace_forget(&run_name);

        // Remove directory if found and still exists
        if let Some(ref path) = workspace_path {
            let p = std::path::Path::new(path);
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(p);
                prefout(&format!("removed workspace {}", path));
            }
        }

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
            let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
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

            // If not initialized, show defaults
            if !config::is_initialized()? {
                let value = match k {
                    "trunk_bookmark" => config::DEFAULT_TRUNK_BOOKMARK.to_string(),
                    "check_command" => String::new(),
                    "max_failures" => config::DEFAULT_MAX_FAILURES.to_string(),
                    _ => unreachable!(),
                };
                println!("{}", value);
                return Ok(());
            }

            let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
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
            let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
            config::set(k, v)?;
            prefout(&format!("{} = {}", k, v));
            Ok(())
        }
        (None, Some(_)) => {
            bail!("cannot set value without key")
        }
    }
}

/// Validate configuration and environment.
pub fn doctor() -> Result<()> {
    println!("jjq doctor:");

    let mut fails = 0u32;
    let mut warns = 0u32;

    // 1. jj repository (already verified by main, so this always passes)
    print_check("ok", "jj repository");

    // 2. jjq initialized
    let initialized = config::is_initialized()?;
    if initialized {
        print_check("ok", "jjq initialized");
    } else {
        print_check("WARN", "jjq not initialized (run 'jjq push' to initialize)");
        warns += 1;
    }

    // 3. trunk bookmark exists
    let trunk_bookmark = config::get_trunk_bookmark()?;
    if jj::bookmark_exists(&trunk_bookmark)? {
        print_check("ok", &format!("trunk bookmark '{}' exists", trunk_bookmark));
    } else {
        print_check("FAIL", &format!("trunk bookmark '{}' does not exist", trunk_bookmark));
        fails += 1;
    }

    // 4. check command configured
    let check_configured = if initialized {
        config::get_check_command()?.is_some()
    } else {
        false
    };
    if check_configured {
        print_check("ok", "check command configured");
    } else {
        print_check("FAIL", "check command not configured");
        print_hint("to fix: jjq config check_command '<command>'");
        fails += 1;
    }

    // 5. run lock
    match lock::lock_state("run")? {
        lock::LockState::Free => print_check("ok", "run lock is free"),
        lock::LockState::HeldAlive(pid) => {
            print_check("WARN", &format!("run lock held by live process (pid {})", pid));
            warns += 1;
        }
        lock::LockState::HeldDead(pid) => {
            print_check("FAIL", &format!("run lock held by dead process (pid {})", pid));
            print_hint(&format!("to fix: rm -rf {}", lock::lock_path("run")?.display()));
            fails += 1;
        }
        lock::LockState::HeldUnknown => {
            print_check("WARN", "run lock held (unknown process)");
            let path = lock::lock_path("run")?;
            print_hint(&format!("if stale: rm -rf {}", path.display()));
            warns += 1;
        }
    }

    // 6. id lock
    match lock::lock_state("id")? {
        lock::LockState::Free => print_check("ok", "id lock is free"),
        lock::LockState::HeldAlive(pid) => {
            print_check("WARN", &format!("id lock held by live process (pid {})", pid));
            warns += 1;
        }
        lock::LockState::HeldDead(pid) => {
            print_check("FAIL", &format!("id lock held by dead process (pid {})", pid));
            print_hint(&format!("to fix: rm -rf {}", lock::lock_path("id")?.display()));
            fails += 1;
        }
        lock::LockState::HeldUnknown => {
            print_check("WARN", "id lock held (unknown process)");
            let path = lock::lock_path("id")?;
            print_hint(&format!("if stale: rm -rf {}", path.display()));
            warns += 1;
        }
    }

    // 7. orphaned workspaces
    let ws_output = jj::workspace_list()?;
    let orphaned: usize = ws_output
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?.trim_end_matches(':');
            if name.starts_with("jjq-run-")
                || name.starts_with("jjq-config-")
                || name.starts_with("jjq-meta-")
            {
                Some(())
            } else {
                None
            }
        })
        .count();
    if orphaned == 0 {
        print_check("ok", "no orphaned workspaces");
    } else {
        print_check("WARN", &format!("{} orphaned workspace(s) found", orphaned));
        print_hint("to fix: jjq clean");
        warns += 1;
    }

    // Summary
    println!();
    if fails == 0 && warns == 0 {
        println!("all checks passed");
    } else {
        let mut parts = Vec::new();
        if fails > 0 {
            parts.push(format!("{} failure(s)", fails));
        }
        if warns > 0 {
            parts.push(format!("{} warning(s)", warns));
        }
        println!("{}", parts.join(", "));
    }

    if fails > 0 {
        Err(ExitError::new(exit_codes::CONFLICT, "doctor found issues").into())
    } else {
        Ok(())
    }
}

fn print_check(status: &str, msg: &str) {
    match status {
        "ok" => println!("   ok  {}", msg),
        "WARN" => println!(" WARN  {}", msg),
        "FAIL" => println!(" FAIL  {}", msg),
        _ => println!("  {}  {}", status, msg),
    }
}

fn print_hint(msg: &str) {
    println!("       {}", msg);
}

/// Remove all jjq workspaces and their directories.
pub fn clean() -> Result<()> {
    let ws_output = jj::workspace_list()?;

    let mut removed = 0u32;
    let mut details = Vec::new();

    for line in ws_output.lines() {
        let ws_name = line.split_whitespace().next().unwrap_or("");
        let ws_name = ws_name.trim_end_matches(':');
        if !ws_name.starts_with("jjq-run-") {
            continue;
        }

        // Extract ID from workspace name
        let ws_id_str = ws_name.strip_prefix("jjq-run-").unwrap_or("000000");
        let plain_id: u32 = ws_id_str.parse().unwrap_or(0);

        // Check if corresponding failed bookmark exists
        let label = if queue::failed_item_exists(plain_id)? {
            format!("failed item {}", plain_id)
        } else {
            "orphaned".to_string()
        };

        // Look up workspace path
        let workspace_path = lookup_workspace_path(plain_id);

        // Forget the workspace
        let _ = jj::workspace_forget(ws_name);

        // Remove directory if found
        if let Some(ref path) = workspace_path {
            let p = std::path::Path::new(path);
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(p);
            }
        }

        let path_info = workspace_path
            .map(|p| format!(" {}", p))
            .unwrap_or_default();
        details.push(format!("  {} ({}){}", ws_name, label, path_info));
        removed += 1;
    }

    if removed == 0 {
        prefout("no workspaces to clean");
    } else {
        let detail_str = details.join("\n");
        prefout(&format!("removed {} workspace(s)\n{}", removed, detail_str));
    }

    Ok(())
}
