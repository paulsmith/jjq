// ABOUTME: Command implementations for jjq CLI.
// ABOUTME: Each function implements one jjq subcommand per the specification.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::env;
use std::fs;
use tempfile::TempDir;

use serde::Serialize;

use crate::config::{self, Strategy};
use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::lock::{self, Lock};
use crate::queue;

#[derive(Serialize)]
struct StatusOutput {
    running: bool,
    queue: Vec<QueueItem>,
    failed: Vec<FailedItem>,
}

#[derive(Serialize)]
struct QueueItem {
    id: u32,
    change_id: String,
    commit_id: String,
    description: String,
}

#[derive(Serialize)]
struct FailedItem {
    id: u32,
    candidate_change_id: String,
    candidate_commit_id: String,
    description: String,
    trunk_commit_id: String,
    workspace_path: String,
    failure_reason: String,
}

/// Check if a workspace name belongs to jjq (and should be cleaned up by doctor/clean).
/// Covers all workspace naming patterns: jjq-run-*, jjq-config-*, jjq-meta-*,
/// jjq-check-*, jjq-hint-*, and bare "jjq{PID}" from init/next_id.
fn is_jjq_workspace(name: &str) -> bool {
    name.starts_with("jjq-") || (name.starts_with("jjq") && name.len() > 3)
}

/// Output with jjq: prefix to stdout.
fn prefout(msg: &str) {
    println!("jjq: {}", msg);
}

/// Output with jjq: prefix to stderr.
fn preferr(msg: &str) {
    eprintln!("jjq: {}", msg);
}

/// Require jjq to be initialized, or error with instructions.
fn require_initialized() -> Result<()> {
    if !config::is_initialized()? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "jjq is not initialized. Run 'jjq init' first.",
        )
        .into());
    }
    Ok(())
}

/// Extract the numeric ID from a bookmark name like "jjq/queue/000042".
fn extract_id_from_bookmark(bookmark: &str) -> u32 {
    bookmark
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
}

/// Extract all jjq-* trailers from a commit description into a map.
/// Trailer format: "jjq-key: value" — returns map of "key" -> "value".
fn extract_trailers(description: &str) -> HashMap<String, String> {
    let mut trailers = HashMap::new();
    for line in description.lines() {
        if let Some(rest) = line.strip_prefix("jjq-")
            && let Some((key, value)) = rest.split_once(": ")
        {
            trailers.insert(key.to_string(), value.trim().to_string());
        }
    }
    trailers
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

/// Initialize jjq in this repository.
pub fn init(trunk: Option<&str>, check: Option<&str>, strategy: &str) -> Result<()> {
    use std::io::{self, IsTerminal};

    // Refuse if already initialized
    if config::is_initialized()? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "jjq is already initialized. Use 'jjq config' to change settings.",
        )
        .into());
    }

    println!("Initializing jjq in this repository.");
    println!();

    let is_tty = io::stdin().is_terminal();

    // Non-interactive mode requires both flags
    if !is_tty && (trunk.is_none() || check.is_none()) {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "--trunk and --check are required in non-interactive mode.",
        )
        .into());
    }

    // Determine trunk bookmark
    let trunk_value = if let Some(t) = trunk {
        t.to_string()
    } else {
        // Auto-detect default from existing bookmarks
        let bookmarks = jj::list_bookmarks().unwrap_or_default();
        let default = if bookmarks.iter().any(|b| b == "main") {
            Some("main")
        } else if bookmarks.iter().any(|b| b == "master") {
            Some("master")
        } else {
            None
        };
        prompt("Trunk bookmark", default, None)?
    };

    // Verify trunk bookmark exists
    if !jj::bookmark_exists(&trunk_value)? {
        if !is_tty {
            return Err(ExitError::new(
                exit_codes::USAGE,
                format!("trunk bookmark '{}' does not exist.", trunk_value),
            )
            .into());
        }
        // Interactive mode: offer to create it
        println!("Bookmark '{}' does not exist.", trunk_value);
        println!("  1) Create it at the parent revision (@-)");
        println!("  2) Create it at a different revset");
        println!("  3) Exit");
        let choice = prompt_choice("Choice", 3)?;
        let rev = match choice {
            1 => "@-".to_string(),
            2 => prompt(
                "Revset",
                None,
                Some("A revset is required (e.g., '@-', 'main', a change ID)."),
            )?,
            _ => {
                return Err(ExitError::new(
                    exit_codes::USAGE,
                    format!("trunk bookmark '{}' does not exist.", trunk_value),
                )
                .into());
            }
        };
        jj::bookmark_create(&trunk_value, &rev)?;
        println!("Created bookmark '{}' at '{}'.", trunk_value, rev);
    }

    // Determine check command
    let check_value = if let Some(c) = check {
        c.to_string()
    } else {
        prompt(
            "Check command",
            None,
            Some("A check command is required (e.g., 'make test', 'cargo test')."),
        )?
    };

    // Initialize metadata branch
    config::initialize()?;

    // Set config values
    config::set("trunk_bookmark", &trunk_value)?;
    config::set("check_command", &check_value)?;

    // Validate and set strategy
    let strategy_val = config::Strategy::try_from(strategy).ok().ok_or_else(|| {
        ExitError::new(
            exit_codes::USAGE,
            format!(
                "invalid strategy: {}\nvalid values: rebase, merge",
                strategy
            ),
        )
    })?;
    config::set("strategy", strategy_val.as_str())?;

    // Configure jj to hide jjq metadata from jj log
    setup_log_filter()?;

    println!();
    println!("Initialized jjq:");
    println!("  trunk_bookmark = {}", trunk_value);
    println!("  check_command  = {}", check_value);
    println!("  strategy       = {}", strategy_val.as_str());
    println!();

    // Run doctor
    println!("Running doctor...");
    doctor()?;

    println!();
    println!("Ready to go! Queue revisions with 'jjq push <revset>'.");

    Ok(())
}

/// Configure jj's revsets.log to exclude jjq metadata from `jj log`.
/// Composes with any existing filter value.
fn setup_log_filter() -> Result<()> {
    let exclude = format!("~ ::{}", config::JJQ_BOOKMARK);

    let value = if let Ok(Some(current)) = jj::config_get("revsets.log") {
        if current.contains(config::JJQ_BOOKMARK) {
            return Ok(());
        }
        format!("({}) {}", current, exclude)
    } else {
        exclude
    };

    jj::config_set_repo("revsets.log", &value)?;
    prefout("configured jj to hide jjq metadata from 'jj log'");
    Ok(())
}

/// Print a prompt and read one line of input, returning the trimmed content.
fn read_prompt(prompt_text: &str) -> Result<String> {
    use std::io::{self, BufRead, Write};
    print!("{}", prompt_text);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Prompt for a value with optional default and optional empty-input hint. Loops until non-empty.
fn prompt(label: &str, default: Option<&str>, empty_hint: Option<&str>) -> Result<String> {
    loop {
        let suffix = match default {
            Some(d) => format!("{} [{}]: ", label, d),
            None => format!("{}: ", label),
        };
        let input = read_prompt(&suffix)?;
        if input.is_empty() {
            if let Some(d) = default {
                return Ok(d.to_string());
            }
            if let Some(hint) = empty_hint {
                println!("{}", hint);
            }
            continue;
        }
        return Ok(input);
    }
}

/// Prompt for a numbered choice (1..=max). Loops until valid.
fn prompt_choice(label: &str, max: u32) -> Result<u32> {
    loop {
        let input = read_prompt(&format!("{} [1-{}]: ", label, max))?;
        if let Ok(n) = input.parse::<u32>()
            && n >= 1
            && n <= max
        {
            return Ok(n);
        }
        println!("Please enter a number between 1 and {}.", max);
    }
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
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!("trunk bookmark '{}' not found", trunk_bookmark),
        )
        .into());
    }

    // Idempotent push: clean up existing queue/failed entries for this change

    // Scan queue bookmarks (one subprocess per bookmark for both IDs)
    let queue_bookmarks = jj::bookmark_list_glob("jjq/queue/??????")?;
    for bookmark in &queue_bookmarks {
        let revset = format!("bookmarks(exact:{})", bookmark);
        let (entry_change_id, entry_commit_id) = jj::resolve_revset_full(&revset)?;
        if entry_commit_id == commit_id {
            let entry_id = extract_id_from_bookmark(bookmark);
            preferr(&format!("revision already queued at {}", entry_id));
            return Err(ExitError::new(exit_codes::USAGE, "revision already queued").into());
        }
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
        let trailers = extract_trailers(&desc);
        if let Some(candidate_change_id) = trailers.get("candidate")
            && *candidate_change_id == change_id
        {
            let entry_id = extract_id_from_bookmark(bookmark);
            jj::bookmark_delete(bookmark)?;
            prefout(&format!("clearing failed entry {}", entry_id));
        }
    }

    // Pre-flight conflict check using headless merge commit.
    // Ensure the temporary commit is always abandoned, even if has_conflicts errors.
    let conflict_check_id = jj::new_rev(&[&trunk_bookmark, revset])?;
    let has_conflicts = match jj::has_conflicts(&conflict_check_id) {
        Ok(v) => {
            jj::abandon(&conflict_check_id)?;
            v
        }
        Err(e) => {
            let _ = jj::abandon(&conflict_check_id);
            return Err(e);
        }
    };

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

    require_initialized()?;

    let id = queue::next_id()?;
    let bookmark = queue::queue_bookmark(id);

    jj::bookmark_create(&bookmark, revset)?;

    let repo_path = jj::repo_root()?;
    prefout(&format!(
        "revision '{}' queued at {} (trunk: {} in {})",
        revset, id, trunk_bookmark, repo_path.display()
    ));

    // Show one-time hint about configuring jj log
    config::maybe_show_log_hint()?;

    Ok(())
}

/// Process queue items.
pub fn run(all: bool, stop_on_failure: bool) -> Result<()> {
    require_initialized()?;

    if all {
        run_all(stop_on_failure)
    } else {
        match run_one()? {
            RunResult::Success => Ok(()),
            RunResult::Empty => Ok(()),
            RunResult::Skipped => Ok(()),
            RunResult::Failure(code, msg) => Err(ExitError::new(code, msg).into()),
        }
    }
}

enum RunResult {
    Success,
    Empty,
    Skipped,
    Failure(i32, String),
}

fn run_all(stop_on_failure: bool) -> Result<()> {
    let mut merged_count = 0u32;
    let mut failed_count = 0u32;
    let mut skipped_count = 0u32;

    loop {
        match run_one()? {
            RunResult::Success => {
                merged_count += 1;
            }
            RunResult::Empty => {
                break;
            }
            RunResult::Skipped => {
                skipped_count += 1;
            }
            RunResult::Failure(_code, msg) => {
                if stop_on_failure {
                    if merged_count > 0 {
                        prefout(&format!(
                            "processed {} item(s) before failure",
                            merged_count
                        ));
                    }
                    return Err(ExitError::new(exit_codes::CONFLICT, msg).into());
                }
                failed_count += 1;
            }
        }
    }

    if merged_count > 0 || failed_count > 0 || skipped_count > 0 {
        if failed_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} failed",
                merged_count, failed_count
            ));
            return Err(ExitError::new(
                exit_codes::PARTIAL,
                format!(
                    "processed {} item(s), {} failed",
                    merged_count, failed_count
                ),
            )
            .into());
        }
        if skipped_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} skipped (empty)",
                merged_count, skipped_count
            ));
        } else {
            prefout(&format!("processed {} item(s)", merged_count));
        }
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
    let strategy = config::get_strategy()?;
    drop(config_lock);

    prefout(&format!(
        "processing queue item {} ({} strategy)",
        id,
        strategy.as_str()
    ));

    // Acquire run lock
    let run_lock = match Lock::acquire("run")? {
        Some(lock) => lock,
        None => {
            preferr("queue runner lock already held");
            return Ok(RunResult::Failure(
                exit_codes::CONFLICT,
                "run lock unavailable".to_string(),
            ));
        }
    };

    // Record trunk commit ID
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;

    // Capture candidate change ID before creating workspace
    let queue_bookmark = queue::queue_bookmark(id);
    let (candidate_change_id, candidate_commit_id) =
        jj::resolve_revset_full(&format!("bookmarks(exact:{})", queue_bookmark))?;

    // Resolve log path before cd-ing into workspace so it stays in the main repo .jj
    let log_path = crate::runlog::log_path()?;

    // Capture original description for rebase success path (before queue bookmark is deleted)
    let candidate_description =
        jj::get_description(&format!("bookmarks(exact:{})", queue_bookmark)).unwrap_or_default();

    // Create workspace — strategy determines how
    let runner_workspace = TempDir::new()?;
    let run_name = format!("jjq-run-{}", queue::format_seq_id(id));

    // For rebase strategy, track all duplicate IDs so we can abandon them all
    let mut rebase_duplicate_ids: Vec<String> = Vec::new();

    match strategy {
        config::Strategy::Merge => {
            jj::workspace_add(
                runner_workspace.path().to_str().unwrap(),
                &run_name,
                &[
                    &format!("bookmarks(exact:{})", trunk_bookmark),
                    &format!("bookmarks(exact:{})", queue_bookmark),
                ],
            )?;
        }
        config::Strategy::Rebase => {
            // Duplicate candidate onto trunk (creates rebased copy without touching original)
            rebase_duplicate_ids = jj::duplicate_onto(
                &format!("bookmarks(exact:{})", queue_bookmark),
                &format!("bookmarks(exact:{})", trunk_bookmark),
            )?;
            // Create workspace on the tip duplicate (last in the list)
            let duplicate_tip = rebase_duplicate_ids.last().unwrap();
            jj::workspace_add(
                runner_workspace.path().to_str().unwrap(),
                &run_name,
                &[duplicate_tip.as_str()],
            )?;
        }
    }

    // Record the workspace path in metadata for later recovery by delete/clean
    record_workspace_metadata(id, runner_workspace.path().to_str().unwrap())?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(runner_workspace.path())?;

    // For rebase strategy, edit the duplicate directly so check artifacts
    // are snapshotted into it (workspace add created an empty commit on top)
    if strategy == config::Strategy::Rebase {
        let parent_rev = jj::resolve_revset(&format!("{}@-", run_name))?;
        jj::edit(&parent_rev)?;
    }

    // Check for conflicts
    let workspace_rev = format!("{}@", run_name);
    if jj::has_conflicts(&workspace_rev)? {
        jj::bookmark_delete(&queue_bookmark)?;
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::describe(
            &workspace_rev,
            &failure_description(
                id,
                "conflicts",
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                runner_workspace.path(),
                &strategy,
            ),
        )?;

        env::set_current_dir(&orig_dir)?;
        let ws_path = runner_workspace.keep();
        drop(run_lock);

        preferr(&format!("merge {} has conflicts, marked as failed", id));
        preferr(&format!("workspace: {}", ws_path.display()));
        preferr("");
        preferr("To resolve:");
        preferr(&format!(
            "  1. Rebase your revision onto {} and resolve conflicts",
            trunk_bookmark
        ));
        preferr("  2. Run: jjq push <fixed-revset>");
        return Ok(RunResult::Failure(
            exit_codes::CONFLICT,
            format!("merge {} has conflicts", id),
        ));
    }

    // Check for empty commit (no changes vs trunk).
    // Compare the workspace tree against trunk: if they match, the candidate
    // adds nothing new and can be skipped.
    let is_empty = jj::trees_match(
        &format!("bookmarks(exact:{})", trunk_bookmark),
        &workspace_rev,
    )?;
    if is_empty {
        jj::bookmark_delete(&queue_bookmark)?;

        // For rebase, abandon all duplicates we created
        if strategy == config::Strategy::Rebase {
            for dup_id in &rebase_duplicate_ids {
                let _ = jj::abandon(dup_id);
            }
        }

        env::set_current_dir(&orig_dir)?;
        jj::workspace_forget(&run_name)?;
        drop(run_lock);

        preferr(&format!(
            "queue item {} is empty (no changes vs {}), skipping",
            id, trunk_bookmark
        ));
        return Ok(RunResult::Skipped);
    }

    jj::describe(&workspace_rev, &format!("WIP: attempting merge {}", id))?;

    // Run check command (log_path resolved before cd to workspace)
    let check_status = crate::runner::run_check_command(&check_command, &log_path)?;

    if !check_status.success() {
        // Print log output (skipping sentinel lines)
        if let Ok(log_contents) = fs::read_to_string(&log_path) {
            for line in log_contents.lines() {
                if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                    eprintln!("{}", line);
                }
            }
        }

        jj::bookmark_delete(&queue_bookmark)?;
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::describe(
            &workspace_rev,
            &failure_description(
                id,
                "check",
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                runner_workspace.path(),
                &strategy,
            ),
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
        return Ok(RunResult::Failure(
            exit_codes::CONFLICT,
            format!("merge {} check failed", id),
        ));
    }

    // Verify trunk hasn't moved
    let current_trunk_commit_id =
        jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;
    if trunk_commit_id != current_trunk_commit_id {
        // For rebase, abandon all duplicates we created
        if strategy == config::Strategy::Rebase {
            for dup_id in &rebase_duplicate_ids {
                let _ = jj::abandon(dup_id);
            }
        }
        env::set_current_dir(&orig_dir)?;
        jj::workspace_forget(&run_name)?;
        drop(run_lock);

        preferr("trunk bookmark moved during run; queue item left in place, re-run to retry");
        return Ok(RunResult::Failure(
            exit_codes::CONFLICT,
            "trunk moved during run".to_string(),
        ));
    }

    // Success path — order matters for crash safety:
    // 1. Move trunk first (most critical)
    // 2. Delete queue bookmark
    // 3. Describe commit
    // For rebase: we tested against a duplicate, now rebase the original to
    // preserve change ID, then move trunk to the rebased original.

    match strategy {
        config::Strategy::Merge => {
            let landed_change_id = jj::resolve_revset("@")?;
            jj::bookmark_move(&trunk_bookmark, &trunk_commit_id, "@")?;
            jj::bookmark_delete(&queue_bookmark)?;
            jj::describe("@", &format!("Success: merge {}", id))?;

            env::set_current_dir(&orig_dir)?;
            jj::workspace_forget(&run_name)?;
            drop(run_lock);

            prefout(&format!(
                "merged {} to {} (now at {})",
                id, trunk_bookmark, landed_change_id
            ));
        }
        config::Strategy::Rebase => {
            // The duplicate passed checks. Now rebase the ORIGINAL candidate
            // onto trunk to preserve its change ID.

            // Return to original directory for rebase operations
            env::set_current_dir(&orig_dir)?;

            // Rebase original candidate (and its descendants) onto trunk
            jj::rebase_branch_onto(
                &candidate_change_id,
                &format!("bookmarks(exact:{})", trunk_bookmark),
            )?;

            // Move trunk to the rebased original (not the duplicate)
            // The candidate_change_id is now rebased onto trunk
            jj::bookmark_move(&trunk_bookmark, &trunk_commit_id, &candidate_change_id)?;
            jj::bookmark_delete(&queue_bookmark)?;

            // Describe the landed commit with trailers
            let desc = format!(
                "{}\n\njjq-sequence: {}\njjq-strategy: rebase",
                candidate_description.trim(),
                id,
            );
            jj::describe(&candidate_change_id, &desc)?;

            // Abandon all duplicates (they were only used for testing)
            for dup_id in &rebase_duplicate_ids {
                jj::abandon(dup_id)?;
            }

            jj::workspace_forget(&run_name)?;
            drop(run_lock);

            prefout(&format!(
                "rebased {} to {} (now at {})",
                id, trunk_bookmark, candidate_change_id
            ));
        }
    }

    Ok(RunResult::Success)
}

fn failure_description(
    id: u32,
    reason: &str,
    candidate_change_id: &str,
    candidate_commit_id: &str,
    trunk_commit_id: &str,
    workspace_path: &std::path::Path,
    strategy: &Strategy,
) -> String {
    format!(
        "Failed: merge {} ({})\n\njjq-candidate: {}\njjq-candidate-commit: {}\njjq-trunk: {}\njjq-workspace: {}\njjq-failure: {}\njjq-strategy: {}",
        id,
        reason,
        candidate_change_id,
        candidate_commit_id,
        trunk_commit_id,
        workspace_path.display(),
        reason,
        strategy.as_str()
    )
}

/// Run check command against a revision in a temporary workspace.
pub fn check(revset: &str, verbose: bool) -> Result<()> {
    // Resolve the revision
    let change_id =
        jj::resolve_revset(revset).map_err(|e| ExitError::new(exit_codes::USAGE, e.to_string()))?;

    // Read check command
    let check_command = match config::get_check_command()? {
        Some(cmd) => cmd,
        None => {
            return Err(ExitError::new(
                exit_codes::USAGE,
                "check_command not configured (use 'jjq config check_command <cmd>')",
            )
            .into());
        }
    };

    prefout(&format!(
        "checking revision {} with: {}",
        change_id, check_command
    ));

    // Resolve log path before changing to workspace directory.
    let log_path = crate::runlog::log_path()?;

    // Create temporary workspace
    let workspace_dir = TempDir::new()?;
    let workspace_name = format!("jjq-check-{}", std::process::id());

    jj::workspace_add(
        workspace_dir.path().to_str().unwrap(),
        &workspace_name,
        &[revset],
    )?;

    let orig_dir = env::current_dir()?;
    env::set_current_dir(workspace_dir.path())?;

    if verbose {
        prefout(&format!("workspace: {}", workspace_dir.path().display()));
        prefout("shell: /bin/sh");
        prefout("env:");
        let mut vars: Vec<(String, String)> = env::vars().collect();
        vars.sort();
        for (key, value) in &vars {
            prefout(&format!("  {}={}", key, value));
        }
    }

    // Run check command
    let check_status = crate::runner::run_check_command(&check_command, &log_path)?;

    // Print log output (skipping sentinel lines)
    if let Ok(log_contents) = fs::read_to_string(&log_path) {
        for line in log_contents.lines() {
            if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                println!("{}", line);
            }
        }
    }

    let success = check_status.success();

    // Always clean up
    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    if success {
        prefout("check passed");
        Ok(())
    } else {
        Err(ExitError::new(exit_codes::CONFLICT, "check failed").into())
    }
}

/// Build a QueueItem by resolving data from the bookmark target.
fn build_queue_item(id: u32) -> Result<QueueItem> {
    let bookmark = queue::queue_bookmark(id);
    let revset = format!("bookmarks(exact:{})", bookmark);
    let (change_id, commit_id) = jj::resolve_revset_full(&revset)?;
    let description = jj::get_description(&revset)?;
    let description = description.lines().next().unwrap_or("").to_string();
    Ok(QueueItem {
        id,
        change_id,
        commit_id,
        description,
    })
}

/// Build a FailedItem by parsing trailers from the bookmark target description.
fn build_failed_item(id: u32) -> Result<FailedItem> {
    let bookmark = queue::failed_bookmark(id);
    let revset = format!("bookmarks(exact:{})", bookmark);
    let desc = jj::get_description(&revset)?;
    let trailers = extract_trailers(&desc);

    let candidate_change_id = trailers.get("candidate").cloned().unwrap_or_default();
    let candidate_commit_id = trailers
        .get("candidate-commit")
        .cloned()
        .unwrap_or_default();
    let trunk_commit_id = trailers.get("trunk").cloned().unwrap_or_default();
    let workspace_path = trailers.get("workspace").cloned().unwrap_or_default();
    let failure_reason = trailers.get("failure").cloned().unwrap_or_default();

    // Resolve original candidate description from the candidate change ID
    let description = if !candidate_change_id.is_empty() {
        jj::get_description(&candidate_change_id)
            .map(|d| d.lines().next().unwrap_or("").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };

    Ok(FailedItem {
        id,
        candidate_change_id,
        candidate_commit_id,
        description,
        trunk_commit_id,
        workspace_path,
        failure_reason,
    })
}

/// Display queue status.
pub fn status(id: Option<&str>, json: bool, resolve: Option<&str>) -> Result<()> {
    // Single-item modes
    if id.is_some() || resolve.is_some() {
        return status_single(id, json, resolve);
    }

    if !config::is_initialized()? {
        if json {
            let output = StatusOutput {
                running: false,
                queue: vec![],
                failed: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            prefout("jjq not initialized. Run 'jjq init' first.");
        }
        return Ok(());
    }

    let running = lock::is_held("run")?;

    let queue_ids = queue::get_queue()?;
    let failed_ids = queue::get_failed()?;

    let queue_items: Vec<QueueItem> = queue_ids
        .iter()
        .map(|&id| build_queue_item(id))
        .collect::<Result<_>>()?;

    let failed_items: Vec<FailedItem> = failed_ids
        .iter()
        .map(|&id| build_failed_item(id))
        .collect::<Result<_>>()?;

    if json {
        let output = StatusOutput {
            running,
            queue: queue_items,
            failed: failed_items,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if running {
            prefout("Run in progress");
            println!();
        }

        if queue_items.is_empty() && failed_items.is_empty() {
            prefout("queue is empty");
            return Ok(());
        }

        if !queue_items.is_empty() {
            prefout("Queued:");
            for item in &queue_items {
                println!("  {}: {} {}", item.id, item.change_id, item.description);
            }
        }

        if !failed_items.is_empty() {
            if !queue_items.is_empty() {
                println!();
            }
            prefout("Failed (recent):");
            for item in &failed_items {
                println!(
                    "  {}: {} {}",
                    item.id, item.candidate_change_id, item.description
                );
            }
        }
    }

    Ok(())
}

fn status_single(id: Option<&str>, json: bool, resolve: Option<&str>) -> Result<()> {
    let (item_id, is_queued) = if let Some(id_str) = id {
        let id = queue::parse_seq_id(id_str)?;
        if queue::queue_item_exists(id)? {
            (id, true)
        } else if queue::failed_item_exists(id)? {
            (id, false)
        } else {
            bail!("item {} not found in queue or failed", id)
        }
    } else if let Some(change_id) = resolve {
        find_by_change_id(change_id)?
    } else {
        unreachable!()
    };

    if is_queued {
        let item = build_queue_item(item_id)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&item)?);
        } else {
            println!("Queue item {}", item.id);
            println!("  Change ID:   {}", item.change_id);
            println!("  Commit ID:   {}", item.commit_id);
            println!("  Description: {}", item.description);
        }
    } else {
        let item = build_failed_item(item_id)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&item)?);
        } else {
            println!("Failed item {}", item.id);
            println!(
                "  Candidate:   {} ({})",
                item.candidate_change_id, item.candidate_commit_id
            );
            println!("  Description: {}", item.description);
            println!("  Failure:     {}", item.failure_reason);
            println!("  Trunk:       {}", item.trunk_commit_id);
            println!("  Workspace:   {}", item.workspace_path);
            println!();
            println!("To resolve:");
            println!("  1. Fix the issue and create a new revision");
            println!("  2. Run: jjq push <fixed-revset>");
        }
    }

    Ok(())
}

/// Find a queue or failed item by candidate change ID.
/// Returns (sequence_id, is_queued).
fn find_by_change_id(change_id: &str) -> Result<(u32, bool)> {
    // Search queue items
    let queue_ids = queue::get_queue()?;
    for id in &queue_ids {
        let bookmark = queue::queue_bookmark(*id);
        let revset = format!("bookmarks(exact:{})", bookmark);
        if let Ok(item_change_id) = jj::resolve_revset(&revset)
            && item_change_id == change_id
        {
            return Ok((*id, true));
        }
    }

    // Search failed items
    let failed_ids = queue::get_failed()?;
    for id in &failed_ids {
        let bookmark = queue::failed_bookmark(*id);
        let revset = format!("bookmarks(exact:{})", bookmark);
        if let Ok(desc) = jj::get_description(&revset) {
            let trailers = extract_trailers(&desc);
            if trailers.get("candidate").map(|s| s.as_str()) == Some(change_id) {
                return Ok((*id, false));
            }
        }
    }

    bail!("no item found with candidate change ID '{}'", change_id)
}

/// Delete an item from queue or failed list.
pub fn delete(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;

    require_initialized()?;

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
            require_initialized()?;
            let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
            let trunk = config::get_trunk_bookmark()?;
            let check = config::get_check_command()?;

            println!("trunk_bookmark = {}", trunk);
            println!(
                "check_command = {}",
                check.unwrap_or_else(|| "(not set)".to_string())
            );
            let strategy = config::get_strategy()?;
            println!("strategy = {}", strategy.as_str());
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
                    "strategy" => config::DEFAULT_STRATEGY.as_str().to_string(),
                    _ => unreachable!(),
                };
                println!("{}", value);
                return Ok(());
            }

            let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
            let value = match k {
                "trunk_bookmark" => config::get_trunk_bookmark()?,
                "check_command" => config::get_check_command()?.unwrap_or_default(),
                "strategy" => config::get_strategy()?.as_str().to_string(),
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
        print_check("FAIL", "jjq not initialized (run 'jjq init')");
        fails += 1;
    }

    // 3. trunk bookmark exists
    let trunk_bookmark = config::get_trunk_bookmark()?;
    if jj::bookmark_exists(&trunk_bookmark)? {
        print_check("ok", &format!("trunk bookmark '{}' exists", trunk_bookmark));
    } else {
        print_check(
            "FAIL",
            &format!("trunk bookmark '{}' does not exist", trunk_bookmark),
        );
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

    // 5. strategy valid
    if initialized {
        match config::get_strategy() {
            Ok(s) => print_check("ok", &format!("strategy: {}", s.as_str())),
            Err(e) => {
                print_check("FAIL", &format!("invalid strategy: {}", e));
                fails += 1;
            }
        }
    }

    // 6. jj log filter hides jjq metadata
    if let Ok(Some(current_log)) = jj::config_get("revsets.log")
        && current_log.contains(config::JJQ_BOOKMARK)
    {
        print_check("ok", "jj log hides jjq metadata");
    } else {
        print_check("WARN", "jj log does not hide jjq metadata");
        print_hint(&format!(
            "to fix: jj config set --repo revsets.log '~ ::{}'",
            config::JJQ_BOOKMARK
        ));
        warns += 1;
    }

    // 7. locks
    match lock::lock_state("run")? {
        lock::LockState::Free => print_check("ok", "run lock is free"),
        lock::LockState::Held => {
            print_check("WARN", "run lock held by another process");
            warns += 1;
        }
    }

    // (id lock)
    match lock::lock_state("id")? {
        lock::LockState::Free => print_check("ok", "id lock is free"),
        lock::LockState::Held => {
            print_check("WARN", "id lock held by another process");
            warns += 1;
        }
    }

    // 8. orphaned workspaces
    let ws_output = jj::workspace_list()?;
    let orphaned: usize = ws_output
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?.trim_end_matches(':');
            is_jjq_workspace(name).then_some(())
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
        if !is_jjq_workspace(ws_name) {
            continue;
        }

        // For run workspaces, extract the queue item ID and look up details.
        // Config/meta workspaces don't correspond to queue items.
        let (label, workspace_path) = if let Some(ws_id_str) = ws_name.strip_prefix("jjq-run-") {
            let plain_id: u32 = ws_id_str.parse().unwrap_or(0);
            let label = if queue::failed_item_exists(plain_id)? {
                format!("failed item {}", plain_id)
            } else {
                "orphaned".to_string()
            };
            (label, lookup_workspace_path(plain_id))
        } else {
            ("orphaned".to_string(), None)
        };

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
