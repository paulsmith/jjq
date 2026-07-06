// ABOUTME: Command implementations for jjq CLI.
// ABOUTME: Each function implements one jjq subcommand per the specification.

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::sync::OnceLock;
use tempfile::TempDir;

use serde::Serialize;

use crate::config::{self, Strategy};
use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::lock::{self, Lock};
use crate::meta;
use crate::queue;

#[derive(Serialize)]
struct StatusOutput {
    runner_active: bool,
    running: Vec<QueueItem>,
    queue: Vec<QueueItem>,
    failed: Vec<FailedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    landed: Vec<LandedItem>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflict_paths: Vec<String>,
}

#[derive(Serialize)]
struct LandedItem {
    id: u32,
    change_id: String,
    description: String,
}

/// Check if a workspace name belongs to jjq (and should be cleaned up by
/// doctor/clean). Matches exactly the names jjq generates — jjq-run-NNNNNN,
/// jjq-meta-<pid>-<seq>, jjq-check-<pid> — plus names left behind by earlier
/// layouts (jjq<pid>, jjq-config-<pid>, jjq-hint-<pid>). Clean destroys
/// whatever this matches, so a user workspace that merely starts with "jjq"
/// (e.g. jjq-experiments) must not match.
fn is_jjq_workspace(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^jjq(-run-\d{6}|-meta-\d+-\d+|-check-\d+|-config-\d+|-hint-\d+|\d+)$").unwrap()
    });
    re.is_match(name)
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
    if let Ok(Some(snap)) = config::snapshot()
        && let Ok(Some(path)) = snap.file(&file_path)
    {
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

/// Best-effort removal of a run's workspace registration and directory.
/// Used when clearing failed items and stale claims; the workspace may
/// already be gone, so nothing here is an error.
fn cleanup_run_workspace(id: u32) {
    let run_name = format!("jjq-run-{}", queue::format_seq_id(id));
    let workspace_path = lookup_workspace_path(id);
    let _ = jj::workspace_forget(&run_name);
    if let Some(ref path) = workspace_path {
        let p = std::path::Path::new(path);
        if p.is_dir() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// Record workspace path in metadata for later recovery by delete/clean.
/// Called while holding the run lock; takes the meta lock inside (run → meta,
/// never the reverse — see src/meta.rs).
fn record_workspace_metadata(id: u32, workspace_path: &str) -> Result<()> {
    meta::with_meta_workspace(|workspace| {
        let dir = workspace.join("workspace");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(queue::format_seq_id(id)), workspace_path)?;
        Ok(format!("Sequence-Id: {}\nWorkspace: {}", id, workspace_path))
    })
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
    doctor(false)?;

    println!();
    println!("Ready to go! Queue revisions with 'jjq push <revset>'.");

    Ok(())
}

/// Configure jj's revsets.log to exclude jjq metadata from `jj log`.
/// Composes with any existing filter value. The bookmarks(exact:) form is
/// load-bearing: jj builds its short-prefixes index from revsets.log, and a
/// bare `jjq/_/_` symbol there makes every jj command warn or fail should
/// the bookmark ever become conflicted, while bookmarks(exact:) tolerates a
/// conflicted ref.
fn setup_log_filter() -> Result<()> {
    let exclude = format!("~ ::{}", meta::meta_revset());

    let value = if let Ok(Some(current)) = jj::config_get("revsets.log") {
        if current.contains(meta::JJQ_BOOKMARK) {
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

    // Idempotent push: clean up existing queue/failed entries for this change.
    // Entries are enumerated by ID and their bookmark names derived — never
    // listed by revset: `jj bookmark list -r` returns every bookmark on the
    // matched commits, including the user's own co-located bookmarks.

    for entry_id in queue::get_queue()? {
        let bookmark = queue::queue_bookmark(entry_id);
        let revset = format!("bookmarks(exact:{})", bookmark);
        let (entry_change_id, entry_commit_id) = jj::resolve_revset_full(&revset)?;
        if entry_commit_id == commit_id {
            preferr(&format!("revision already queued at {}", entry_id));
            return Err(ExitError::new(exit_codes::USAGE, "revision already queued").into());
        }
        if entry_change_id == change_id {
            jj::bookmark_delete(&bookmark)?;
            prefout(&format!("replacing queued entry {}", entry_id));
        }
    }

    // Running claims: an item claimed by an active runner must not be
    // replaced or cleared out from under it. Holding the run lock across the
    // mutation (not just probing it) keeps a starting runner from re-claiming
    // the entry mid-replacement; with the lock unavailable the runner is live
    // and the push is refused.
    for entry_id in queue::get_running()? {
        let bookmark = queue::running_bookmark(entry_id);
        let revset = format!("bookmarks(exact:{})", bookmark);
        let (entry_change_id, entry_commit_id) = jj::resolve_revset_full(&revset)?;
        if entry_change_id == change_id || entry_commit_id == commit_id {
            let Some(_run_lock) = Lock::acquire("run")? else {
                return Err(ExitError::new(
                    exit_codes::USAGE,
                    format!("revision is currently being processed (item {})", entry_id),
                )
                .into());
            };
            jj::bookmark_delete(&bookmark)?;
            prefout(&format!("replacing interrupted entry {}", entry_id));
            cleanup_run_workspace(entry_id);
        }
    }

    // Failed entries: match on the jjq-candidate trailer.
    for entry_id in queue::get_failed()? {
        let bookmark = queue::failed_bookmark(entry_id);
        let desc = jj::get_description(&format!("bookmarks(exact:{})", bookmark))?;
        let trailers = extract_trailers(&desc);
        if let Some(candidate_change_id) = trailers.get("candidate")
            && *candidate_change_id == change_id
        {
            jj::bookmark_delete(&bookmark)?;
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

    let repo_path = jj::primary_workspace_root()?;
    prefout(&format!(
        "revision '{}' queued at {} (trunk: {} in {})",
        revset, id, trunk_bookmark, repo_path.display()
    ));

    // Show one-time hint about configuring jj log
    config::maybe_show_log_hint()?;

    Ok(())
}

/// Configuration for one run invocation, read once per batch.
struct RunConfig {
    trunk_bookmark: String,
    check_command: String,
    strategy: Strategy,
}

/// Process queue items.
pub fn run(all: bool, stop_on_failure: bool) -> Result<()> {
    require_initialized()?;

    // Take the run lock before reading queue or config state, so a concurrent
    // runner exits immediately instead of contending with the active one.
    let run_lock = match Lock::acquire("run")? {
        Some(lock) => lock,
        None => {
            return Err(
                ExitError::new(exit_codes::CONFLICT, "another queue runner is active").into(),
            );
        }
    };

    // Read configuration once, right after taking the run lock: the whole
    // batch lands against a single trunk/check/strategy generation
    // (per-batch consistency is intended — a concurrent `jjq config` set
    // applies to the next run, not mid-batch). A missing check command is a
    // configuration error reported once, before touching any queue item.
    let snapshot =
        config::snapshot()?.ok_or_else(|| anyhow!("jjq metadata missing; run 'jjq doctor'"))?;
    let run_config = RunConfig {
        trunk_bookmark: snapshot.trunk_bookmark()?,
        check_command: match snapshot.check_command()? {
            Some(cmd) => cmd,
            None => {
                return Err(ExitError::new(
                    exit_codes::CONFLICT,
                    "check_command not configured (use 'jjq config check_command <cmd>')",
                )
                .into());
            }
        },
        strategy: snapshot.strategy()?,
    };

    if all {
        run_all(&run_lock, &run_config, stop_on_failure)
    } else {
        match run_one(&run_lock, &run_config)? {
            RunResult::Success => Ok(()),
            RunResult::Empty => Ok(()),
            RunResult::Skipped => Ok(()),
            // run_one already reported the failure in detail
            RunResult::Failure => Err(ExitError::code_only(exit_codes::CONFLICT).into()),
        }
    }
}

enum RunResult {
    Success,
    Empty,
    Skipped,
    /// The item failed (conflict, check failure, trunk moved, or removed
    /// mid-run). run_one reports the details; in single-run mode this maps
    /// to exit code CONFLICT.
    Failure,
}

fn run_all(run_lock: &Lock, run_config: &RunConfig, stop_on_failure: bool) -> Result<()> {
    let mut merged_count = 0u32;
    let mut failed_count = 0u32;
    let mut skipped_count = 0u32;

    loop {
        match run_one(run_lock, run_config)? {
            RunResult::Success => {
                merged_count += 1;
            }
            RunResult::Empty => {
                break;
            }
            RunResult::Skipped => {
                skipped_count += 1;
            }
            RunResult::Failure => {
                if stop_on_failure {
                    if merged_count > 0 {
                        prefout(&format!(
                            "processed {} item(s) before failure",
                            merged_count
                        ));
                    }
                    return Err(ExitError::code_only(exit_codes::CONFLICT).into());
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
            return Err(ExitError::code_only(exit_codes::PARTIAL).into());
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

/// Move a stale running claim back to its queue position (same ID). Callers
/// must know the claim is orphaned: either they hold the run lock, or they
/// verified the run lock is free.
fn recover_claim(id: u32) -> Result<()> {
    let running_bookmark = queue::running_bookmark(id);
    if !queue::queue_item_exists(id)? {
        jj::bookmark_create(
            &queue::queue_bookmark(id),
            &format!("bookmarks(exact:{})", running_bookmark),
        )?;
    }
    jj::bookmark_delete(&running_bookmark)
}

/// Process the next queue item. The caller must hold the run lock, passed
/// here as evidence, and supplies the batch-wide configuration.
fn run_one(_run_lock: &Lock, run_config: &RunConfig) -> Result<RunResult> {
    // Under the run lock any existing claim is an orphan from a crashed
    // runner; move it back to its queue position. Recovery keeps the same
    // ID, so FIFO order is preserved (the lowest ID processes next anyway).
    // The crashed runner also leaves its jjq-run-<id> workspace registered —
    // it must go now, or re-claiming the item dies on the name collision.
    for stale_id in queue::get_running()? {
        cleanup_run_workspace(stale_id);
        recover_claim(stale_id)?;
        prefout(&format!("recovered interrupted item {}", stale_id));
    }

    let id = match queue::next_item()? {
        Some(id) => id,
        None => {
            prefout("queue is empty");
            return Ok(RunResult::Empty);
        }
    };

    let trunk_bookmark = &run_config.trunk_bookmark;
    let strategy = run_config.strategy;

    prefout(&format!(
        "processing queue item {} ({} strategy)",
        id,
        strategy.as_str()
    ));

    // Claim the item: the queue -> running rename, made under the run lock,
    // marks it as being processed. delete/requeue/push refuse to touch a
    // running item while the run lock is held, so user commands cannot
    // silently lose against this runner (see pre-land verification below for
    // the out-of-band case).
    let running_bookmark = queue::running_bookmark(id);
    jj::bookmark_create(
        &running_bookmark,
        &format!("bookmarks(exact:{})", queue::queue_bookmark(id)),
    )?;
    jj::bookmark_delete(&queue::queue_bookmark(id))?;

    // Record trunk commit ID
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;

    // Capture candidate change ID before creating workspace
    let (candidate_change_id, candidate_commit_id) =
        jj::resolve_revset_full(&format!("bookmarks(exact:{})", running_bookmark))?;

    // Log path lives in the shared .jj so `jjq tail` sees it from any workspace
    let log_path = crate::runlog::log_path()?;

    // Capture original description for rebase success path (before the claim is deleted)
    let candidate_description =
        jj::get_description(&format!("bookmarks(exact:{})", running_bookmark)).unwrap_or_default();

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
                    &format!("bookmarks(exact:{})", running_bookmark),
                ],
            )?;
        }
        config::Strategy::Rebase => {
            // Duplicate candidate onto trunk (creates rebased copy without touching original)
            rebase_duplicate_ids = jj::duplicate_onto(
                &format!("bookmarks(exact:{})", running_bookmark),
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

    let workspace_path = runner_workspace.path().to_path_buf();

    // For rebase strategy, edit the duplicate directly so check artifacts
    // are snapshotted into it (workspace add created an empty commit on top)
    if strategy == config::Strategy::Rebase {
        let parent_rev = jj::resolve_revset(&format!("{}@-", run_name))?;
        jj::edit(&workspace_path, &parent_rev)?;
    }

    // Check for conflicts
    let workspace_rev = format!("{}@", run_name);
    if jj::has_conflicts(&workspace_rev)? {
        let conflicts = jj::conflict_paths(&workspace_rev)?;
        // Create the failed entry before deleting the claim so a crash
        // between the two duplicates the item instead of losing it.
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::bookmark_delete(&running_bookmark)?;
        jj::describe(
            &workspace_rev,
            &failure_description(
                id,
                "conflicts",
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                &workspace_path,
                &strategy,
                &conflicts,
            ),
        )?;

        let _ = runner_workspace.keep();

        preferr(&format!("merge {} has conflicts, marked as failed", id));
        preferr(&format!("  candidate: {}", candidate_change_id));
        if !conflicts.is_empty() {
            preferr(&format!("  conflicting files: {}", conflicts.join(", ")));
        }
        preferr("");
        preferr("To resolve:");
        preferr(&format!(
            "  jj rebase -r {} -d {}",
            candidate_change_id, trunk_bookmark
        ));
        preferr(&format!(
            "  # resolve conflicts in {}",
            candidate_change_id
        ));
        preferr(&format!("  jjq push {}", candidate_change_id));
        return Ok(RunResult::Failure);
    }

    // Check for empty commit (no changes vs trunk).
    // Compare the workspace tree against trunk: if they match, the candidate
    // adds nothing new and can be skipped.
    let is_empty = jj::trees_match(
        &format!("bookmarks(exact:{})", trunk_bookmark),
        &workspace_rev,
    )?;
    if is_empty {
        jj::bookmark_delete(&running_bookmark)?;

        // For rebase, abandon all duplicates we created
        if strategy == config::Strategy::Rebase {
            for dup_id in &rebase_duplicate_ids {
                let _ = jj::abandon(dup_id);
            }
        }

        jj::workspace_forget(&run_name)?;

        preferr(&format!(
            "queue item {} is empty (no changes vs {}), skipping",
            id, trunk_bookmark
        ));
        return Ok(RunResult::Skipped);
    }

    jj::describe(&workspace_rev, &format!("WIP: attempting merge {}", id))?;

    // Run check command with the runner workspace as its working directory
    let check_status =
        crate::runner::run_check_command(&run_config.check_command, &workspace_path, &log_path)?;

    // The check may have modified files in the runner workspace (build
    // artifacts, formatters). Snapshot them into the workspace commit so the
    // revset-based operations below see the post-check tree.
    jj::snapshot_workspace(&workspace_path)?;

    if !check_status.success() {
        // Print log output (skipping sentinel lines)
        if let Ok(log_contents) = fs::read_to_string(&log_path) {
            for line in log_contents.lines() {
                if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                    eprintln!("{}", line);
                }
            }
        }

        // Failed entry before claim delete: a crash between the two
        // duplicates the item instead of losing it.
        jj::bookmark_create(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::bookmark_delete(&running_bookmark)?;
        jj::describe(
            &workspace_rev,
            &failure_description(
                id,
                "check",
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                &workspace_path,
                &strategy,
                &[],
            ),
        )?;

        let _ = runner_workspace.keep();

        preferr(&format!("merge {} failed check, marked as failed", id));
        preferr(&format!("  candidate: {}", candidate_change_id));
        preferr("");
        preferr("To resolve:");
        preferr(&format!("  # fix the issue in {}", candidate_change_id));
        preferr(&format!("  jjq push {}", candidate_change_id));
        return Ok(RunResult::Failure);
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
        jj::workspace_forget(&run_name)?;

        // The item was not at fault; the claim goes back to its queue
        // position so the next run retries it.
        recover_claim(id)?;
        preferr("trunk bookmark moved during run; queue item left in place, re-run to retry");
        return Ok(RunResult::Failure);
    }

    // Pre-land verification: the claim can vanish despite the run lock
    // (out-of-band bookmark surgery, or a stale-claim delete that raced this
    // runner's startup). Landing anyway would silently override that
    // removal, so discard the temporary work instead. This runs immediately
    // before the landing sequence: before rebase_branch_onto rewrites the
    // user's original change (rebase) and before trunk moves (merge).
    if !queue::running_item_exists(id)? {
        if strategy == config::Strategy::Rebase {
            for dup_id in &rebase_duplicate_ids {
                let _ = jj::abandon(dup_id);
            }
        }
        jj::workspace_forget(&run_name)?;

        preferr(&format!(
            "item {} was removed while being processed; discarding result",
            id
        ));
        return Ok(RunResult::Failure);
    }

    // Success path — order matters for crash safety:
    // 1. Move trunk first (most critical)
    // 2. Delete the running claim
    // 3. Describe commit
    // For rebase: we tested against a duplicate, now rebase the original to
    // preserve change ID, then move trunk to the rebased original.

    match strategy {
        config::Strategy::Merge => {
            // The workspace-qualified revset resolves to the merge commit,
            // whose tree was snapshotted above after the check ran.
            let landed_change_id = jj::resolve_revset(&workspace_rev)?;
            jj::bookmark_move(trunk_bookmark, &trunk_commit_id, &workspace_rev)?;
            if !queue::running_item_exists(id)? {
                preferr(&format!(
                    "note: item {} was removed during landing; it landed anyway",
                    id
                ));
            }
            jj::bookmark_delete(&running_bookmark)?;
            jj::describe(&workspace_rev, &format!("Success: merge {}", id))?;

            jj::workspace_forget(&run_name)?;

            prefout(&format!(
                "merged {} to {} (now at {})",
                id, trunk_bookmark, landed_change_id
            ));
        }
        config::Strategy::Rebase => {
            // The duplicate passed checks. Now rebase the ORIGINAL candidate
            // onto trunk to preserve its change ID.

            // Rebase original candidate (and its descendants) onto trunk
            jj::rebase_branch_onto(
                &candidate_change_id,
                &format!("bookmarks(exact:{})", trunk_bookmark),
            )?;

            // Move trunk to the rebased original (not the duplicate)
            // The candidate_change_id is now rebased onto trunk
            jj::bookmark_move(trunk_bookmark, &trunk_commit_id, &candidate_change_id)?;
            if !queue::running_item_exists(id)? {
                preferr(&format!(
                    "note: item {} was removed during landing; it landed anyway",
                    id
                ));
            }
            jj::bookmark_delete(&running_bookmark)?;

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
    conflict_paths: &[String],
) -> String {
    let mut desc = format!(
        "Failed: merge {} ({})\n\njjq-candidate: {}\njjq-candidate-commit: {}\njjq-trunk: {}\njjq-workspace: {}\njjq-failure: {}\njjq-strategy: {}",
        id,
        reason,
        candidate_change_id,
        candidate_commit_id,
        trunk_commit_id,
        workspace_path.display(),
        reason,
        strategy.as_str()
    );
    if !conflict_paths.is_empty() {
        desc.push_str(&format!("\njjq-conflicts: {}", conflict_paths.join(",")));
    }
    desc
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

    // Log path lives in the shared .jj so `jjq tail` sees it from any workspace
    let log_path = crate::runlog::log_path()?;

    // Hold the check lock in shared mode for the workspace's lifetime, so
    // concurrent checks coexist but `jjq clean` (which probes it
    // exclusively) cannot destroy a live check workspace. Lock order: check
    // takes only this lock; clean takes run then check; no path takes run
    // while holding check.
    let _check_lock = match Lock::acquire_shared("check")? {
        Some(lock) => lock,
        None => {
            return Err(ExitError::new(
                exit_codes::CONFLICT,
                "jjq clean is in progress; retry when it finishes",
            )
            .into());
        }
    };

    // Create temporary workspace
    let workspace_dir = TempDir::new()?;
    let workspace_name = format!("jjq-check-{}", std::process::id());

    jj::workspace_add(
        workspace_dir.path().to_str().unwrap(),
        &workspace_name,
        &[revset],
    )?;

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

    // Run check command with the temporary workspace as its working directory
    let check_status =
        crate::runner::run_check_command(&check_command, workspace_dir.path(), &log_path)?;

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
    jj::workspace_forget(&workspace_name)?;

    if success {
        prefout("check passed");
        Ok(())
    } else {
        Err(ExitError::new(exit_codes::CONFLICT, "check failed").into())
    }
}

/// Build a QueueItem by resolving data from a bookmark target.
fn build_item_at_bookmark(id: u32, bookmark: &str) -> Result<QueueItem> {
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

/// Build a QueueItem for a queued entry.
fn build_queue_item(id: u32) -> Result<QueueItem> {
    build_item_at_bookmark(id, &queue::queue_bookmark(id))
}

/// Build a QueueItem for a running (claimed) entry.
fn build_running_item(id: u32) -> Result<QueueItem> {
    build_item_at_bookmark(id, &queue::running_bookmark(id))
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
    let conflict_paths = trailers
        .get("conflicts")
        .map(|s| s.split(',').map(|p| p.to_string()).collect())
        .unwrap_or_default();

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
        conflict_paths,
    })
}

/// Find recently landed items by scanning trunk ancestors for jjq trailers.
/// Returns up to `limit` items, most recent first.
fn get_recently_landed(trunk_bookmark: &str, limit: usize) -> Result<Vec<LandedItem>> {
    // Scan recent trunk ancestors for commits landed by jjq. We look for
    // jjq-sequence trailers (rebase strategy) or "Success: merge" descriptions
    // (merge strategy). Scan more than limit since not every ancestor is jjq-landed.
    let scan_count = (limit * 10).max(50);
    let revset = format!(
        "ancestors(bookmarks(exact:\"{}\"), {})",
        trunk_bookmark, scan_count
    );
    let template = "change_id.short() ++ \"\\t\" ++ description ++ \"\\x00\"";
    let output = jj::run_ok(&["log", "-r", &revset, "--no-graph", "-T", template])?;

    let mut items = Vec::new();
    for block in output.split('\x00') {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let (change_id, description) = match block.split_once('\t') {
            Some(pair) => pair,
            None => continue,
        };

        let trailers = extract_trailers(description);

        if let Some(seq_str) = trailers.get("sequence") {
            // Rebase strategy: description has the original text + trailers
            if let Ok(id) = seq_str.parse::<u32>() {
                let desc_line = description
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                items.push(LandedItem {
                    id,
                    change_id: change_id.to_string(),
                    description: desc_line,
                });
            }
        } else if let Some(rest) = description.trim().strip_prefix("Success: merge ") {
            // Merge strategy: description is "Success: merge {id}"
            if let Ok(id) = rest.trim().parse::<u32>() {
                items.push(LandedItem {
                    id,
                    change_id: change_id.to_string(),
                    description: description.lines().next().unwrap_or("").to_string(),
                });
            }
        }

        if items.len() >= limit {
            break;
        }
    }

    Ok(items)
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
                runner_active: false,
                running: vec![],
                queue: vec![],
                failed: vec![],
                landed: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            prefout("jjq not initialized. Run 'jjq init' first.");
        }
        return Ok(());
    }

    let runner_active = lock::is_held("run")?;

    let running_ids = queue::get_running()?;
    let queue_ids = queue::get_queue()?;
    let failed_ids = queue::get_failed()?;

    let running_items: Vec<QueueItem> = running_ids
        .iter()
        .map(|&id| build_running_item(id))
        .collect::<Result<_>>()?;

    let queue_items: Vec<QueueItem> = queue_ids
        .iter()
        .map(|&id| build_queue_item(id))
        .collect::<Result<_>>()?;

    let failed_items: Vec<FailedItem> = failed_ids
        .iter()
        .map(|&id| build_failed_item(id))
        .collect::<Result<_>>()?;

    let trunk_bookmark = config::get_trunk_bookmark()?;
    let landed_items = get_recently_landed(&trunk_bookmark, 5).unwrap_or_default();

    if json {
        let output = StatusOutput {
            runner_active,
            running: running_items,
            queue: queue_items,
            failed: failed_items,
            landed: landed_items,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if runner_active {
            prefout("Run in progress");
            println!();
        }

        if running_items.is_empty()
            && queue_items.is_empty()
            && failed_items.is_empty()
            && landed_items.is_empty()
        {
            prefout("queue is empty");
            return Ok(());
        }

        if !running_items.is_empty() {
            prefout("Running:");
            for item in &running_items {
                println!("  {}: {} {}", item.id, item.change_id, item.description);
            }
        }

        if !queue_items.is_empty() {
            if !running_items.is_empty() {
                println!();
            }
            prefout("Queued:");
            for item in &queue_items {
                println!("  {}: {} {}", item.id, item.change_id, item.description);
            }
        }

        if !failed_items.is_empty() {
            if !running_items.is_empty() || !queue_items.is_empty() {
                println!();
            }
            prefout("Failed (recent):");
            for item in &failed_items {
                if item.conflict_paths.is_empty() {
                    println!(
                        "  {}: {} {}",
                        item.id, item.candidate_change_id, item.description
                    );
                } else {
                    println!(
                        "  {}: {} {} (conflicts: {})",
                        item.id,
                        item.candidate_change_id,
                        item.description,
                        item.conflict_paths.join(", ")
                    );
                }
            }
        }

        if !landed_items.is_empty() {
            if !running_items.is_empty() || !queue_items.is_empty() || !failed_items.is_empty() {
                println!();
            }
            prefout("Landed (recent):");
            for item in &landed_items {
                println!("  {}: {} {}", item.id, item.change_id, item.description);
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
        } else if queue::running_item_exists(id)? {
            bail!("item {} is currently being processed (run in progress)", id)
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
            if !item.conflict_paths.is_empty() {
                println!("  Conflicts:   {}", item.conflict_paths.join(", "));
            }
            println!("  Trunk:       {}", item.trunk_commit_id);
            println!();
            let trunk_bookmark = config::get_trunk_bookmark().unwrap_or_else(|_| "main".into());
            if item.failure_reason == "conflicts" {
                println!("To resolve:");
                println!(
                    "  jj rebase -r {} -d {}",
                    item.candidate_change_id, trunk_bookmark
                );
                println!("  # resolve conflicts in {}", item.candidate_change_id);
                println!("  jjq push {}", item.candidate_change_id);
            } else {
                println!("To resolve:");
                println!("  # fix the issue in {}", item.candidate_change_id);
                println!("  jjq push {}", item.candidate_change_id);
            }
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

/// Re-push a failed item back onto the queue.
pub fn requeue(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;

    require_initialized()?;

    if !queue::failed_item_exists(id)? {
        if queue::queue_item_exists(id)? {
            return Err(
                ExitError::new(exit_codes::USAGE, format!("item {} is already queued", id)).into(),
            );
        }
        if queue::running_item_exists(id)? {
            // Hold the run lock across the recovery (not just a probe): a
            // starting runner could otherwise re-claim the item mid-recovery.
            let Some(_run_lock) = Lock::acquire("run")? else {
                return Err(ExitError::new(
                    exit_codes::USAGE,
                    format!(
                        "item {} is being processed by a running queue runner (jjq requeue can only act on it after the run finishes)",
                        id
                    ),
                )
                .into());
            };
            // Stale claim from a crashed runner: recover it to its queue
            // position, which is what requeue would achieve anyway.
            cleanup_run_workspace(id);
            recover_claim(id)?;
            prefout(&format!("recovered interrupted item {}; it is queued again", id));
            return Ok(());
        }
        return Err(
            ExitError::new(exit_codes::USAGE, format!("failed item {} not found", id)).into(),
        );
    }

    // Build the failed item to get candidate info
    let item = build_failed_item(id)?;
    let candidate_change_id = &item.candidate_change_id;

    if candidate_change_id.is_empty() {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!("failed item {} has no candidate change ID", id),
        )
        .into());
    }

    // Resolve the candidate — it may have been rebased since the failure
    let revset = candidate_change_id.as_str();
    let (_change_id, _commit_id) = jj::resolve_revset_full(revset)
        .map_err(|e| ExitError::new(exit_codes::USAGE, format!("candidate not found: {}", e)))?;

    // Get trunk bookmark for conflict check
    let trunk_bookmark = config::get_trunk_bookmark()?;
    if !jj::bookmark_exists(&trunk_bookmark)? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!("trunk bookmark '{}' not found", trunk_bookmark),
        )
        .into());
    }

    // Pre-flight conflict check against current trunk
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
            "revision '{}' still conflicts with {}",
            revset, trunk_bookmark
        ));
        preferr(&format!(
            "rebase onto {} and resolve conflicts before requeuing",
            trunk_bookmark
        ));
        return Err(
            ExitError::new(exit_codes::CONFLICT, "revision conflicts with trunk").into(),
        );
    }

    // Allocate new queue ID and create queue bookmark
    let new_id = queue::next_id()?;
    let new_bookmark = queue::queue_bookmark(new_id);
    jj::bookmark_create(&new_bookmark, revset)?;

    // Clean up the failed entry and its workspace
    let padded = queue::format_seq_id(id);
    let run_name = format!("jjq-run-{}", padded);
    let workspace_path = lookup_workspace_path(id);

    jj::bookmark_delete(&queue::failed_bookmark(id))?;
    let _ = jj::workspace_forget(&run_name);

    if let Some(ref path) = workspace_path {
        let p = std::path::Path::new(path);
        if p.is_dir() {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    let repo_path = jj::primary_workspace_root()?;
    prefout(&format!(
        "requeued failed item {} as {} (trunk: {} in {})",
        id, new_id, trunk_bookmark, repo_path.display()
    ));

    Ok(())
}

/// Delete an item from queue or failed list.
pub fn delete(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;

    require_initialized()?;

    // Check queue first
    if queue::queue_item_exists(id)? {
        jj::bookmark_delete(&queue::queue_bookmark(id))?;
        // A crash between claim-create and queue-delete leaves both
        // bookmarks; remove the leftover claim too, or the next run's
        // recovery resurrects and lands the item just deleted. If a live
        // runner is mid-claim, its pre-land verification discards the
        // result — the delete still wins.
        if queue::running_item_exists(id)? {
            jj::bookmark_delete(&queue::running_bookmark(id))?;
        }
        prefout(&format!("deleted queued item {}", id));
        return Ok(());
    }

    // A running item is claimed by a queue runner. With the run lock held
    // the runner is live and deleting would be silently overridden when it
    // lands the item; with the lock free the claim is an orphan from a
    // crashed runner and safe to remove. The lock is held across the
    // removal (not just probed) so a starting runner cannot re-claim the
    // item mid-delete.
    if queue::running_item_exists(id)? {
        let Some(_run_lock) = Lock::acquire("run")? else {
            return Err(ExitError::new(
                exit_codes::USAGE,
                format!(
                    "item {} is being processed by a running queue runner (jjq delete can only remove it after the run finishes)",
                    id
                ),
            )
            .into());
        };

        jj::bookmark_delete(&queue::running_bookmark(id))?;
        prefout(&format!(
            "deleted interrupted item {} (stale claim from a crashed runner)",
            id
        ));

        // Clean up whatever the crashed runner left behind, as for failed items.
        cleanup_run_workspace(id);

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

/// Get or set configuration. Reads pin one metadata snapshot and take no
/// lock; writes serialize inside config::set via the meta lock.
pub fn config(key: Option<&str>, value: Option<&str>) -> Result<()> {
    match (key, value) {
        (None, None) => {
            // Show all config from a single snapshot
            require_initialized()?;
            let snapshot = config::snapshot()?
                .ok_or_else(|| anyhow!("jjq metadata missing; run 'jjq doctor'"))?;

            println!("trunk_bookmark = {}", snapshot.trunk_bookmark()?);
            println!(
                "check_command = {}",
                snapshot
                    .check_command()?
                    .unwrap_or_else(|| "(not set)".to_string())
            );
            println!("strategy = {}", snapshot.strategy()?.as_str());
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
            let Some(snapshot) = config::snapshot()? else {
                let value = match k {
                    "trunk_bookmark" => config::DEFAULT_TRUNK_BOOKMARK.to_string(),
                    "check_command" => String::new(),
                    "strategy" => config::DEFAULT_STRATEGY.as_str().to_string(),
                    _ => unreachable!(),
                };
                println!("{}", value);
                return Ok(());
            };

            let value = match k {
                "trunk_bookmark" => snapshot.trunk_bookmark()?,
                "check_command" => snapshot.check_command()?.unwrap_or_default(),
                "strategy" => snapshot.strategy()?.as_str().to_string(),
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

/// Validate configuration and environment. With `fix`, first repair a
/// conflicted metadata bookmark, then run the checks to verify the repair.
pub fn doctor(fix: bool) -> Result<()> {
    if fix {
        fix_conflicted_meta()?;
    }

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

    // 3. no conflicted jjq bookmarks (concurrent out-of-band moves conflict
    // a bookmark, making the queue item or metadata unreadable)
    match conflicted_jjq_bookmarks() {
        Ok(conflicted) if conflicted.is_empty() => {
            print_check("ok", "no conflicted jjq bookmarks");
        }
        Ok(conflicted) => {
            for name in &conflicted {
                print_check("FAIL", &format!("bookmark '{}' is conflicted", name));
                if name == meta::JJQ_BOOKMARK {
                    print_hint("to fix: jjq doctor --fix");
                } else {
                    // Queue/failed bookmarks point at user commits; picking a
                    // side automatically could land the wrong revision.
                    print_hint(&format!(
                        "to fix: jjq delete {}, then push the intended revision again",
                        extract_id_from_bookmark(name)
                    ));
                }
                fails += 1;
            }
        }
        Err(e) => {
            print_check("FAIL", &format!("cannot list jjq bookmarks: {}", e));
            fails += 1;
        }
    }

    // Config checks share one metadata snapshot; failure to take it (e.g. a
    // conflicted metadata bookmark) is itself a doctor failure.
    let snapshot = if initialized {
        match config::snapshot() {
            Ok(Some(snap)) => Some(snap),
            Ok(None) => {
                print_check("FAIL", "jjq metadata bookmark missing");
                fails += 1;
                None
            }
            Err(e) => {
                print_check("FAIL", &format!("cannot read jjq metadata: {}", e));
                fails += 1;
                None
            }
        }
    } else {
        None
    };

    // 4. trunk bookmark exists
    let trunk_bookmark = match &snapshot {
        Some(snap) => snap.trunk_bookmark()?,
        None => config::DEFAULT_TRUNK_BOOKMARK.to_string(),
    };
    if jj::bookmark_exists(&trunk_bookmark)? {
        print_check("ok", &format!("trunk bookmark '{}' exists", trunk_bookmark));
    } else {
        print_check(
            "FAIL",
            &format!("trunk bookmark '{}' does not exist", trunk_bookmark),
        );
        fails += 1;
    }

    // 5. check command configured
    let check_configured = match &snapshot {
        Some(snap) => snap.check_command()?.is_some(),
        None => false,
    };
    if check_configured {
        print_check("ok", "check command configured");
    } else {
        print_check("FAIL", "check command not configured");
        print_hint("to fix: jjq config check_command '<command>'");
        fails += 1;
    }

    // 6. strategy valid
    if let Some(snap) = &snapshot {
        match snap.strategy() {
            Ok(s) => print_check("ok", &format!("strategy: {}", s.as_str())),
            Err(e) => {
                print_check("FAIL", &format!("invalid strategy: {}", e));
                fails += 1;
            }
        }
    }

    // 7. sequence counter readable
    if let Some(snap) = &snapshot {
        match snap.file("last_id") {
            Ok(Some(contents)) if contents.trim().parse::<u32>().is_ok() => {
                print_check("ok", &format!("sequence counter at {}", contents.trim()));
            }
            Ok(Some(contents)) => {
                print_check(
                    "FAIL",
                    &format!("sequence counter (last_id) unparseable: {:?}", contents.trim()),
                );
                fails += 1;
            }
            Ok(None) => {
                print_check("FAIL", "sequence counter (last_id) missing from metadata branch");
                print_hint("metadata branch may be corrupted; pushes will fail until repaired");
                fails += 1;
            }
            Err(e) => {
                print_check("FAIL", &format!("cannot read sequence counter: {}", e));
                fails += 1;
            }
        }
    }

    // 8. jj log filter hides jjq metadata. Either form hides it, but the
    // old bare-symbol form breaks jj's short-prefixes index (and with it
    // every jj command) the moment the bookmark becomes conflicted, so it
    // gets a migration warning.
    let current_log = jj::config_get("revsets.log").ok().flatten();
    match current_log {
        Some(ref value) if value.contains(&meta::meta_revset()) => {
            print_check("ok", "jj log hides jjq metadata");
        }
        Some(ref value) if value.contains(meta::JJQ_BOOKMARK) => {
            print_check("ok", "jj log hides jjq metadata");
            print_check(
                "WARN",
                "revsets.log names the jjq metadata bookmark as a bare symbol",
            );
            print_hint(
                "the bare form breaks jj commands if the bookmark becomes conflicted; bookmarks(exact:) tolerates that",
            );
            print_hint(&format!(
                "to fix: jj config set --repo revsets.log '~ ::{}'",
                meta::meta_revset()
            ));
            warns += 1;
        }
        _ => {
            print_check("WARN", "jj log does not hide jjq metadata");
            print_hint(&format!(
                "to fix: jj config set --repo revsets.log '~ ::{}'",
                meta::meta_revset()
            ));
            warns += 1;
        }
    }

    // 9. locks
    match lock::lock_state("run")? {
        lock::LockState::Free => print_check("ok", "run lock is free"),
        lock::LockState::Held => {
            print_check("WARN", "run lock held by another process");
            warns += 1;
        }
    }

    // (meta lock)
    match lock::lock_state("meta")? {
        lock::LockState::Free => print_check("ok", "meta lock is free"),
        lock::LockState::Held => {
            print_check("WARN", "meta lock held by another process");
            warns += 1;
        }
    }

    // Stale running claims: with the run lock free, jjq/running/* bookmarks
    // are leftovers from a crashed runner. Reported only when present, since
    // they are transient (the next run requeues them).
    if !lock::is_held("run")? {
        for id in queue::get_running()? {
            print_check(
                "WARN",
                &format!(
                    "interrupted run left item {} claimed; the next run will requeue it",
                    id
                ),
            );
            warns += 1;
        }
    }

    // 10. orphaned workspaces
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

/// Bookmark names in the jjq namespace that are conflicted. A conflicted
/// bookmark can emit one line per underlying ref (local plus remote-tracking),
/// so names are deduped; other bookmarks sharing a commit with a jjq bookmark
/// also show up in the listing and are filtered out by prefix.
fn conflicted_jjq_bookmarks() -> Result<Vec<String>> {
    let revset = format!(
        "{} | bookmarks(glob:\"jjq/queue/*\") | bookmarks(glob:\"jjq/running/*\") | bookmarks(glob:\"jjq/failed/*\")",
        meta::meta_revset()
    );
    let output = jj::run_ok_with_config(
        jj::NO_SHORT_PREFIXES,
        &[
            "bookmark",
            "list",
            "-r",
            &revset,
            "-T",
            "name ++ if(conflict, \" CONFLICTED\") ++ \"\\n\"",
        ],
    )?;
    let mut names: Vec<String> = output
        .lines()
        .filter_map(|line| line.strip_suffix(" CONFLICTED"))
        .filter(|name| name.starts_with("jjq/"))
        .map(|name| name.to_string())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Commit IDs the metadata bookmark points at: one for a healthy bookmark,
/// one per conflict side for a conflicted one, none when jjq is
/// uninitialized. Deduped, preserving jj's log order (newest first).
fn meta_bookmark_sides() -> Result<Vec<String>> {
    let output = jj::run_ok_with_config(
        jj::NO_SHORT_PREFIXES,
        &[
            "log",
            "-r",
            &meta::meta_revset(),
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )?;
    let mut seen = std::collections::HashSet::new();
    Ok(output
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|commit| seen.insert(commit.to_string()))
        .map(|commit| commit.to_string())
        .collect())
}

/// Repair a conflicted jjq metadata bookmark in place: keep the side with
/// the highest sequence counter (last_id) and abandon the rest. Only the
/// metadata bookmark is auto-repaired — its sides are jjq's own metadata
/// commits — while conflicted queue/failed bookmarks point at user commits
/// and keep their manual hints.
///
/// Every jj invocation here carries a per-invocation short-prefixes override:
/// a conflicted bookmark named bare in revsets.log can degrade jj revset
/// resolution (observed behavior varies by jj version), and the override
/// sidesteps that without mutating the user's repo config (a crash
/// mid-repair must not destroy their settings).
fn fix_conflicted_meta() -> Result<()> {
    // The repair moves the metadata bookmark, so it holds the same lock as
    // every other metadata writer.
    let _lock = meta::acquire()?;

    // Gate on the same ref-conflict flag doctor's detection uses: a
    // delete-vs-move divergence is conflicted with only ONE remaining add,
    // so side count alone would wrongly report nothing to repair.
    if !conflicted_jjq_bookmarks()?
        .iter()
        .any(|name| name == meta::JJQ_BOOKMARK)
    {
        prefout(&format!(
            "--fix: metadata bookmark ({}) is not conflicted; nothing to repair",
            meta::JJQ_BOOKMARK
        ));
        return Ok(());
    }

    let sides = meta_bookmark_sides()?;
    if sides.is_empty() {
        prefout(&format!(
            "--fix: metadata bookmark ({}) has no remaining targets; nothing to repair",
            meta::JJQ_BOOKMARK
        ));
        return Ok(());
    }

    prefout(&format!(
        "repairing conflicted metadata bookmark ({})",
        meta::JJQ_BOOKMARK
    ));

    // Read the sequence counter on every side. The winner is the side with
    // the highest last_id (first side wins ties), so the counter can only
    // keep or grow its value and no sequence IDs are ever reused.
    let last_ids: Vec<Option<u32>> = sides
        .iter()
        .map(|commit| {
            jj::run_ok_with_config(
                jj::NO_SHORT_PREFIXES,
                &["file", "show", "last_id", "-r", commit],
            )
            .ok()
            .and_then(|contents| contents.trim().parse::<u32>().ok())
        })
        .collect();
    let winner_idx = last_ids
        .iter()
        .enumerate()
        .filter_map(|(i, id)| id.map(|n| (i, n)))
        .max_by(|(ia, na), (ib, nb)| na.cmp(nb).then(ib.cmp(ia)))
        .map(|(i, _)| i);

    let Some(winner_idx) = winner_idx else {
        // No side is recognizable metadata; picking one would be guessing.
        return Err(ExitError::new(
            exit_codes::CONFLICT,
            format!(
                "cannot repair {bm}: no conflict side has a readable sequence counter (last_id)\n\
                 inspect the sides and pick one manually:\n  \
                 jj --config '{cfg}' log -r '{rs}' --no-graph\n  \
                 jj --config '{cfg}' bookmark set '{bm}' -r <commit-id> --allow-backwards",
                bm = meta::JJQ_BOOKMARK,
                cfg = jj::NO_SHORT_PREFIXES,
                rs = meta::meta_revset(),
            ),
        )
        .into());
    };
    let winner = &sides[winner_idx];

    for (i, commit) in sides.iter().enumerate() {
        let counter = match last_ids[i] {
            Some(n) => format!("last_id={}", n),
            None => "last_id unreadable".to_string(),
        };
        let role = if i == winner_idx { "keep" } else { "discard" };
        prefout(&format!("  side {} ({}): {}", &commit[..12], counter, role));
    }

    // Move the bookmark to the winner. --allow-backwards is required — jj
    // cannot order conflict sides — and safe here: this is an explicit,
    // user-invoked repair, and the sides are jjq's own metadata commits.
    jj::run_quiet_with_config(
        jj::NO_SHORT_PREFIXES,
        &[
            "bookmark",
            "set",
            meta::JJQ_BOOKMARK,
            "-r",
            winner,
            "--allow-backwards",
        ],
    )?;

    // Abandon the losing sides, discarding their metadata writes (config
    // changes / workspace records). A loser inside the winner's ancestry is
    // already part of the kept history; abandoning it would rewrite the
    // winner itself. All losers go in ONE abandon invocation so jj handles
    // nested losers (one descending from another) atomically instead of the
    // first abandon rebasing the rest out from under the loop.
    let mut losers: Vec<&str> = Vec::new();
    for (i, commit) in sides.iter().enumerate() {
        if i == winner_idx {
            continue;
        }
        let in_winner_history = !jj::run_ok_with_config(
            jj::NO_SHORT_PREFIXES,
            &[
                "log",
                "-r",
                &format!("{} & ::{}", commit, winner),
                "--no-graph",
                "-T",
                "commit_id",
            ],
        )?
        .trim()
        .is_empty();
        if in_winner_history {
            prefout(&format!(
                "  kept {} (already in the winning side's history)",
                &commit[..12]
            ));
            continue;
        }
        losers.push(commit);
    }
    if !losers.is_empty() {
        let mut args = vec!["abandon"];
        args.extend_from_slice(&losers);
        jj::run_quiet_with_config(jj::NO_SHORT_PREFIXES, &args)?;
    }

    // Fail fast if the repair did not take; the checks that follow
    // re-validate everything on top of this.
    config::snapshot()?;

    prefout(
        "repaired: kept the highest sequence counter, so no IDs are reused; \
         discarded the losing sides' metadata writes (config changes / workspace records)",
    );
    println!();
    Ok(())
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
    // Hold the run lock for the whole cleanup: an active runner's workspace
    // must not be destroyed out from under its check process. Lock order is
    // run, then check (and run -> meta elsewhere; see src/meta.rs) — no path
    // takes run while holding check, so the order stays acyclic.
    let _run_lock = match Lock::acquire("run")? {
        Some(lock) => lock,
        None => {
            return Err(ExitError::new(
                exit_codes::CONFLICT,
                "a queue runner is active; not cleaning (re-run when it finishes)",
            )
            .into());
        }
    };

    // Probe the check lock exclusively: `jjq check` holds it shared for its
    // workspace's lifetime, so acquisition fails while any check is active.
    let _check_lock = match Lock::acquire("check")? {
        Some(lock) => lock,
        None => {
            return Err(ExitError::new(
                exit_codes::CONFLICT,
                "a check is active; not cleaning (re-run when it finishes)",
            )
            .into());
        }
    };

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
        // Meta/check workspaces don't correspond to queue items.
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
