// ABOUTME: Command implementations for jjq CLI.
// ABOUTME: Each function implements one jjq subcommand per the specification.

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use tempfile::TempDir;

use serde::{Deserialize, Serialize};

use crate::config::{self, Strategy};
use crate::exit_codes::{self, ExitError};
use crate::history::{self, Event, kind};
use crate::jj;
use crate::lock::{self, Lock};
use crate::meta;
use crate::paths;
use crate::queue;

#[derive(Serialize)]
struct StatusOutput {
    runner_active: bool,
    running: Vec<QueueItem>,
    queue: Vec<QueueItem>,
    failed: Vec<FailedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    landed: Vec<LandedItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    active_checks: Vec<ActiveCheckStatus>,
}

#[derive(Serialize)]
struct ActiveCheckStatus {
    id: u32,
    run_token: String,
    predicted_base: String,
    speculative: bool,
    workspace_path: String,
    log_path: String,
    phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    process_start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    elapsed_ms: u64,
}

impl From<ActiveCheck> for ActiveCheckStatus {
    fn from(check: ActiveCheck) -> Self {
        Self {
            id: check.id,
            run_token: check.run_token,
            predicted_base: check.predicted_base,
            speculative: check.speculative,
            workspace_path: check.workspace_path,
            log_path: check.log_path,
            phase: check.phase,
            process_start: check.process_start,
            pid: check.pid,
            elapsed_ms: check.elapsed_ms,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ActiveCheck {
    id: u32,
    run_token: String,
    predicted_base: String,
    speculative: bool,
    workspace_path: String,
    log_path: String,
    phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(default)]
    published_at_ms: u64,
    #[serde(default)]
    started_at_ms: u64,
    #[serde(default)]
    elapsed_ms: u64,
    #[serde(default = "default_true")]
    gate_opened: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct QueueItem {
    id: u32,
    change_id: String,
    commit_id: String,
    description: String,
    /// The single-revision landing policy exemption, so operators can see a
    /// deliberate stack before the item fails.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    allow_stack: bool,
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
    /// Recorded by `jjq reject`. Empty means no reason exists: the failed
    /// item is still pending in the fix loop rather than terminal.
    #[serde(skip_serializing_if = "String::is_empty")]
    reject_reason: String,
    /// The single-revision landing policy exemption, recorded at push time
    /// so the fixer can tell a deliberate stack from an accidental one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    allow_stack: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflict_paths: Vec<String>,
}

#[derive(Serialize)]
struct LandedItem {
    id: u32,
    change_id: String,
    description: String,
}

#[derive(Serialize)]
struct HistorySubmission {
    sequence_id: u32,
    events: Vec<Event>,
}

#[derive(Serialize)]
struct HistoryOutput {
    version: u32,
    submissions: Vec<HistorySubmission>,
}

/// Check if a workspace name belongs to jjq (and should be cleaned up by
/// doctor/clean). Matches exactly the names jjq generates — jjq-run-NNNNNN,
/// the persistent jjq-meta workspace, jjq-meta-<pid>-<seq>, jjq-check-<pid> —
/// plus names left behind by earlier
/// layouts (jjq<pid>, jjq-config-<pid>, jjq-hint-<pid>). Clean destroys
/// whatever this matches, so a user workspace that merely starts with "jjq"
/// (e.g. jjq-experiments) must not match.
fn is_jjq_workspace(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^jjq(-run-\d{6}(-[A-Za-z0-9-]+)?|-meta|-meta-\d+-\d+|-check-\d+|-config-\d+|-hint-\d+|\d+)$").unwrap()
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

fn history_for_revision(id: u32, kind: &'static str, revset: &str) -> Result<Event> {
    let mut event = Event::new(id, kind);
    let (change_id, commit_id) = jj::resolve_revset_full(revset)?;
    event.candidate_change_id = Some(change_id);
    event.candidate_commit_id = Some(commit_id);
    Ok(event)
}

fn record_history(event: Event) -> Result<()> {
    // The bookmark mutation is authoritative and has already completed when
    // callers arrive here. A history write error is still reported because a
    // silently missing audit trail would be surprising, but it never rolls
    // back or changes queue state.
    history::record(event)
}

#[allow(clippy::too_many_arguments)]
fn record_cancellation_terminal(
    id: u32,
    candidate_change_id: &str,
    candidate_commit_id: &str,
    trunk_commit_id: Option<&str>,
    strategy: Option<Strategy>,
    elapsed_ms: Option<u128>,
) -> Result<()> {
    let existing = history::read(id)?;
    let mut events = Vec::new();
    for event_kind in [kind::CANCEL_REQUESTED, kind::CANCELED] {
        if existing.iter().any(|event| event.kind == event_kind) {
            continue;
        }
        let mut event = Event::new(id, event_kind);
        event.candidate_change_id = Some(candidate_change_id.to_string());
        event.candidate_commit_id = Some(candidate_commit_id.to_string());
        event.trunk_commit_id = trunk_commit_id.map(str::to_string);
        event.strategy = strategy.map(|value| value.as_str().to_string());
        event.elapsed_ms = elapsed_ms;
        events.push(event);
    }
    history::record_many(&events)
}

/// Complete a marker left beside queued state before any runner claim. Caller
/// holds publication.lock, so a concurrent cancel/claim has one winner.
fn terminalize_queued_cancellation(id: u32) -> Result<()> {
    let bookmark = queue::queue_bookmark(id);
    let (candidate_change_id, candidate_commit_id) =
        jj::resolve_revset_full(&format!("bookmarks(exact:{})", bookmark))?;
    let trunk_commit_id = config::get_trunk_bookmark()
        .ok()
        .and_then(|trunk| jj::get_commit_id(&format!("bookmarks(exact:{})", trunk)).ok());
    record_cancellation_terminal(
        id,
        &candidate_change_id,
        &candidate_commit_id,
        trunk_commit_id.as_deref(),
        None,
        None,
    )?;
    jj::bookmark_delete(&bookmark)?;
    remove_cancellation(id);
    prefout(&format!("completed cancellation of queued item {}", id));
    Ok(())
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
    let run_name = lookup_workspace_name(id)
        .unwrap_or_else(|| format!("jjq-run-{}", queue::format_seq_id(id)));
    let workspace_path = lookup_workspace_path(id);
    let _ = jj::workspace_forget(&run_name);
    if let Some(ref path) = workspace_path {
        let p = std::path::Path::new(path);
        // A dead runner may have left its check child alive. Forgetting the
        // jj workspace lets recovery proceed, but do not delete that child's
        // current directory while its token-scoped active record exists.
        let active_child = active_checks().iter().any(|check| {
            check.workspace_path == p.display().to_string() && active_check_is_live(check)
        });
        if p.is_dir() && !active_child {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// Delete a failed bookmark and its archived check output.
fn remove_failed_entry(id: u32) -> Result<()> {
    jj::bookmark_delete(&queue::failed_bookmark(id))?;
    crate::runlog::remove_failed(id)
}

fn lookup_workspace_name(id: u32) -> Option<String> {
    let path = format!("workspace/{}.name", queue::format_seq_id(id));
    config::snapshot()
        .ok()
        .flatten()?
        .file(&path)
        .ok()
        .flatten()
        .map(|name| name.trim().to_string())
}

fn active_check_path(run_token: &str, id: u32) -> Result<PathBuf> {
    Ok(paths::active_checks_dir()?
        .join(run_token)
        .join(format!("{:06}.json", id)))
}

fn cancellation_path(id: u32) -> Result<PathBuf> {
    Ok(paths::cancellations_dir()?.join(queue::format_seq_id(id)))
}

fn cancellation_requested(id: u32) -> bool {
    cancellation_path(id)
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Publish an ID-scoped intent durably. Sequence IDs are never reused, so an
/// intent remains unambiguous across runner crashes and recovery.
fn publish_cancellation(id: u32) -> Result<bool> {
    let path = cancellation_path(id)?;
    if path.exists() {
        return Ok(false);
    }
    let parent = path.parent().expect("cancellation path has parent");
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = fs::File::create(&temporary)?;
    file.write_all(format!("{}\n", unix_time_ms()).as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, &path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(true)
}

fn remove_cancellation(id: u32) {
    if let Ok(path) = cancellation_path(id) {
        let _ = fs::remove_file(path);
    }
}

/// Publish a complete active-check record with rename, so `status --json`
/// never observes a half-written JSON file.
fn publish_active_check(check: &ActiveCheck) -> Result<()> {
    let path = active_check_path(&check.run_token, check.id)?;
    let parent = path.parent().expect("active check path has parent");
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec(check)?)?;
    file.sync_all()?;
    fs::rename(temporary, &path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn remove_active_check(run_token: &str, id: u32) {
    if let Ok(path) = active_check_path(run_token, id) {
        let _ = fs::remove_file(path);
    }
}

// The gate supervisor detects parent death every 20ms and has a 4s wall-clock
// deadline. Keep PID-less records for 5s, strictly longer than that timeout,
// so a crash before
// publication cannot race recovery; the user command cannot run without gate.
const ACTIVE_CHECK_SPAWN_GRACE_MS: u64 = 5_000;

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

struct NativeProcessInfo {
    start: String,
    zombie: bool,
}

#[cfg(target_os = "macos")]
fn native_process_info(pid: u32) -> Result<NativeProcessInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    // macOS may stop returning BSD info for an unreaped zombie even though
    // kill(pid, 0) still succeeds. In that state there is no executable
    // worker left to protect, so classify it as dead/zombie as well.
    if read == 0 {
        return Ok(NativeProcessInfo {
            start: String::new(),
            zombie: true,
        });
    }
    if read != size {
        bail!("cannot inspect process start identity for pid {}", pid);
    }
    Ok(NativeProcessInfo {
        start: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
        zombie: info.pbi_status == libc::SZOMB,
    })
}

#[cfg(target_os = "linux")]
fn native_process_info(pid: u32) -> Result<NativeProcessInfo> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid))?;
    let fields = stat
        .rsplit_once(") ")
        .ok_or_else(|| anyhow!("invalid /proc stat for pid {}", pid))?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let state = fields
        .first()
        .ok_or_else(|| anyhow!("missing process state for pid {}", pid))?;
    let start = fields
        .get(19)
        .ok_or_else(|| anyhow!("missing process start time for pid {}", pid))?;
    Ok(NativeProcessInfo {
        start: (*start).to_string(),
        zombie: *state == "Z",
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_process_info(pid: u32) -> Result<NativeProcessInfo> {
    bail!(
        "process start identity is unsupported for pid {} on this platform",
        pid
    )
}

fn process_start_identity(pid: u32) -> Result<String> {
    Ok(native_process_info(pid)?.start)
}

fn active_check_is_live(check: &ActiveCheck) -> bool {
    match check.pid {
        Some(pid) if !process_is_alive(pid) => false,
        Some(pid) => match &check.process_start {
            Some(expected) => native_process_info(pid)
                .map(|current| !current.zombie && current.start == *expected)
                .unwrap_or(true),
            // Legacy active records without a start identity remain
            // conservative rather than risking a live worker directory.
            None => true,
        },
        None => unix_time_ms().saturating_sub(check.published_at_ms) <= ACTIVE_CHECK_SPAWN_GRACE_MS,
    }
}

fn refuse_if_orphan_checks_live(action: &str) -> Result<()> {
    let live = active_checks()
        .into_iter()
        .filter(active_check_is_live)
        .map(|check| {
            format!(
                "{} (pid {})",
                check.id,
                check
                    .pid
                    .map_or_else(|| "pending".to_string(), |pid| pid.to_string())
            )
        })
        .collect::<Vec<_>>();
    if live.is_empty() {
        return Ok(());
    };
    Err(ExitError::new(
        exit_codes::CONFLICT,
        format!(
            "checks from an interrupted runner are still active for item(s) {}; not {}",
            live.join(", "),
            action
        ),
    )
    .into())
}

fn clear_active_checks() {
    if let Ok(dir) = paths::active_checks_dir() {
        let _ = fs::remove_dir_all(dir);
    }
}

fn prune_dead_attempt_state() {
    clear_active_checks();
    if let Ok(dir) = paths::jjq_dir() {
        let _ = fs::remove_dir_all(dir.join("run-logs"));
        let _ = fs::remove_dir_all(dir.join("start-gates"));
    }
}

fn remove_attempt_log(path: &std::path::Path) {
    let _ = fs::remove_file(path);
    if let Some(parent) = path.parent()
        && fs::read_dir(parent)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
    {
        let _ = fs::remove_dir(parent);
    }
}

fn active_checks() -> Vec<ActiveCheck> {
    let Ok(dir) = paths::active_checks_dir() else {
        return vec![];
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut checks = entries
        .filter_map(|entry| entry.ok())
        .flat_map(|entry| fs::read_dir(entry.path()).into_iter().flatten())
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|contents| serde_json::from_slice::<ActiveCheck>(&contents).ok())
        .collect::<Vec<_>>();
    let now = unix_time_ms();
    for check in &mut checks {
        let started = if check.started_at_ms == 0 {
            check.published_at_ms
        } else {
            check.started_at_ms
        };
        check.elapsed_ms = now.saturating_sub(started);
        check.phase = if cancellation_requested(check.id) {
            "cancelling".to_string()
        } else {
            "checking".to_string()
        };
    }
    checks.sort_by_key(|check| check.id);
    checks
}

/// Initialize jjq in this repository.
pub fn init(trunk: Option<&str>, check: Option<&str>, strategy: &str) -> Result<()> {
    use std::io::{self, IsTerminal};

    // Refuse a completed initialization, but allow retrying the recoverable
    // bare metadata-root state left if an earlier init died before its first
    // workspace commit. The locked initializer verifies that state again.
    if config::is_initialized()? {
        let has_sequence_counter = match config::snapshot()? {
            Some(snapshot) => snapshot.file("last_id")?.is_some(),
            None => false,
        };
        if has_sequence_counter {
            return Err(ExitError::new(
                exit_codes::USAGE,
                "jjq is already initialized. Use 'jjq config' to change settings.",
            )
            .into());
        }
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

    // Validate all values before writing the complete initial configuration.
    let strategy_val = config::Strategy::try_from(strategy).ok().ok_or_else(|| {
        ExitError::new(
            exit_codes::USAGE,
            format!(
                "invalid strategy: {}\nvalid values: rebase, merge",
                strategy
            ),
        )
    })?;
    config::initialize(&trunk_value, &check_value, strategy_val)?;

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

/// Count revisions the candidate would land atop trunk and apply the
/// single-revision policy. Returns Ok(()) to proceed. `trunk_commit` is the
/// pinned trunk commit ID (not a bookmark name: `bookmarks(exact:)` on a
/// commit ID silently matches unrelated short-prefix bookmarks); callers
/// pass the candidate's pinned commit ID too, so the count never depends on
/// working-copy state.
fn check_landing_policy(candidate_commit: &str, trunk_commit: &str, exempt: bool) -> Result<()> {
    let revset = format!("{}..{}", trunk_commit, candidate_commit);
    let output = jj::run_ok_ignore_working_copy(&[
        "log",
        "-r",
        &revset,
        "--no-graph",
        "-T",
        "\"x\"",
    ])?;
    let n = output.trim().len();
    if n == 0 {
        return Err(ExitError::new(exit_codes::USAGE, "nothing to land atop trunk").into());
    }
    if n > 1 && !exempt && config::get_require_single_revision()? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!(
                "{} revisions atop trunk; squash and re-push (or push --allow-stack)",
                n
            ),
        )
        .into());
    }
    Ok(())
}

/// Push a revision onto the merge queue.
pub fn push(revset: &str, allow_stack: bool) -> Result<()> {
    // Resolve both change ID and commit ID
    let (change_id, commit_id) = jj::resolve_revset_full(revset)
        .map_err(|e| ExitError::new(exit_codes::USAGE, e.to_string()))?;

    require_initialized()?;
    let _mutation_lock = Lock::acquire_blocking("mutation")?;

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

    // Single-revision landing policy, checked synchronously so the submitter
    // gets immediate feedback. The zero-revision refusal applies regardless
    // of config or exemption: there is never a reason to queue nothing.
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;
    check_landing_policy(&commit_id, &trunk_commit_id, allow_stack)?;

    // An exact duplicate push must report the documented USAGE error, not a
    // conflict: if trunk has advanced incompatibly since the identical
    // commit was queued, the pre-flight merge below would fail first and
    // return CONFLICT instead of "already queued". Check the queue namespace
    // for the exact commit before the merge; replacement transitions still
    // happen after the conflict check so a conflicting re-push cannot clear
    // a prior entry.
    {
        let state = queue::state_snapshot()?;
        for entry in state.queued()? {
            if entry.commit_id == commit_id {
                preferr(&format!("revision already queued at {}", entry.id));
                return Err(ExitError::new(exit_codes::USAGE, "revision already queued").into());
            }
        }
    }

    // A live runner's claim must refuse the push with the documented USAGE
    // error, not a conflict: if trunk has advanced incompatibly since the
    // runner claimed this change, the pre-flight merge below would fail
    // first and return CONFLICT. Only the refusal is ordered here; the
    // claim's actual replacement still happens after the conflict check so
    // a conflicting re-push cannot clear a prior entry. A runner holding
    // the run lock is processing this exact claim right now, so refusing
    // needs no lock (the runner cannot finish the claim and delete the
    // bookmark without observing the same conflict at landing).
    {
        let state = queue::state_snapshot()?;
        for entry in state.running()? {
            if entry.change_id == change_id || entry.commit_id == commit_id {
                if lock::is_held("run")? {
                    return Err(ExitError::new(
                        exit_codes::USAGE,
                        format!(
                            "revision is currently being processed (item {})",
                            entry.id
                        ),
                    )
                    .into());
                }
            }
        }
    }

    // Pre-flight conflict check using headless merge commit, before any
    // prior queue/failed entry for this change is cleared: a re-push that
    // now conflicts must not silently remove the previously queued or
    // failed entry.
    // Test the pinned commit, not the revset: a concurrent rewrite (or a
    // snapshot amending @) would otherwise test a different revision than
    // the one queued below. The temporary commit is always abandoned.
    let conflict_check_id = jj::new_rev(&[&trunk_bookmark, &commit_id])?;
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


    // Replacement transitions serialize with the runner's claim window on
    // publication.lock (the same lock the claim takes), so a runner cannot
    // claim an entry between the snapshot below and its deletion, and
    // history cannot observe a cleared entry without its replaced event.
    // The run lock is only ever taken before publication, so the running
    // loop below re-acquires publication per entry.
    let publication_lock = Lock::acquire_blocking("publication")?;
    let state = queue::state_snapshot()?;

    // A matching running claim needs the run lock across the whole
    // replacement, not just the claim's own deletion: without it, a runner
    // that starts after the queued entry is deleted (publication released)
    // can recover the claim back to the queue and re-claim the superseded
    // commit while push has already recorded its replaced event. Taking
    // run first also preserves the lock order run → publication — a
    // starting runner holds run and waits on publication, never the
    // reverse, so this cannot deadlock. With run unavailable the runner is
    // live and processing the claim; refuse as before.
    let matching_running: Vec<_> = state
        .running()?
        .into_iter()
        .filter(|entry| entry.change_id == change_id || entry.commit_id == commit_id)
        .collect();
    let mut run_lock = None;
    if !matching_running.is_empty() {
        run_lock = Some(Lock::acquire("run")?.ok_or_else(|| {
            let id = matching_running[0].id;
            ExitError::new(
                exit_codes::USAGE,
                format!("revision is currently being processed (item {})", id),
            )
        })?);
    }

    // Idempotent push: clean up existing queue/failed entries for this change.
    for entry in state.queued()? {
        let bookmark = queue::queue_bookmark(entry.id);
        if entry.commit_id == commit_id {
            preferr(&format!("revision already queued at {}", entry.id));
            return Err(ExitError::new(exit_codes::USAGE, "revision already queued").into());
        }
        if entry.change_id == change_id {
            // The snapshot was taken before publication was acquired: a
            // runner may have claimed this item in between, so re-check.
            if !queue::queue_item_exists(entry.id)? {
                drop(publication_lock);
                return Err(ExitError::new(
                    exit_codes::USAGE,
                    format!("revision is currently being processed (item {})", entry.id),
                )
                .into());
            }
            jj::bookmark_delete(&bookmark)?;
            let mut event = Event::new(entry.id, kind::REPLACED);
            event.candidate_change_id = Some(entry.change_id);
            event.candidate_commit_id = Some(entry.commit_id);
            record_history(event)?;
            prefout(&format!("replacing queued entry {}", entry.id));
        }
    }

    // Failed entries: match on the jjq-candidate trailer.
    for entry in state.failed()? {
        let trailers = extract_trailers(&entry.description);
        if let Some(candidate_change_id) = trailers.get("candidate")
            && *candidate_change_id == change_id
        {
            let mut event = Event::new(entry.id, kind::REPLACED);
            event.candidate_change_id = Some(candidate_change_id.clone());
            event.candidate_commit_id = trailers.get("candidate-commit").cloned();
            remove_failed_entry(entry.id)?;
            record_history(event)?;
            prefout(&format!("clearing failed entry {}", entry.id));
        }
    }

    // Running claims. We hold both the run lock (acquired above, so no
    // runner is live) and publication. Re-read each entry's state: a
    // runner may have recovered the claim back to the queue before we got
    // here (replacing it then follows the queue semantics).
    for entry in matching_running {
        let bookmark = queue::running_bookmark(entry.id);
        if !queue::running_item_exists(entry.id)? {
            // The claim was recovered to the queue (or already resolved
            // by another mutation) after the snapshot. Replace the
            // recovered queue entry here under both locks.
            if queue::queue_item_exists(entry.id)? {
                jj::bookmark_delete(&queue::queue_bookmark(entry.id))?;
                let mut event = Event::new(entry.id, kind::REPLACED);
                event.candidate_change_id = Some(entry.change_id);
                event.candidate_commit_id = Some(entry.commit_id);
                record_history(event)?;
                prefout(&format!("replacing recovered entry {}", entry.id));
            }
            cleanup_run_workspace(entry.id);
            continue;
        }
        jj::bookmark_delete(&bookmark)?;
        let mut event = Event::new(entry.id, kind::REPLACED);
        event.candidate_change_id = Some(entry.change_id);
        event.candidate_commit_id = Some(entry.commit_id);
        record_history(event)?;
        prefout(&format!("replacing interrupted entry {}", entry.id));
        cleanup_run_workspace(entry.id);
    }
    drop(run_lock);
    drop(publication_lock);

    // Publication couples the authoritative queue bookmark to its first
    // durable event. A runner holds the same lock while claiming, so history
    // cannot observe a newly claimed submission without its queued event.
    let publication_lock = Lock::acquire_blocking("publication")?;
    let id = queue::next_id()?;
    let bookmark = queue::queue_bookmark(id);

    jj::bookmark_create_internal(&bookmark, &commit_id)?;
    let mut event = Event::new(id, kind::QUEUED);
    event.candidate_change_id = Some(change_id.clone());
    event.candidate_commit_id = Some(commit_id.clone());
    event.trunk_commit_id = Some(jj::get_commit_id(&format!(
        "bookmarks(exact:{})",
        trunk_bookmark
    ))?);
    event.allow_stack = allow_stack.then_some(true);
    record_history(event)?;
    drop(publication_lock);

    let repo_path = jj::primary_workspace_root()?;
    prefout(&format!(
        "revision '{}' queued at {} (trunk: {} in {})",
        revset,
        id,
        trunk_bookmark,
        repo_path.display()
    ));

    move_working_copy_off_queued(&change_id)?;

    // Show one-time hint about configuring jj log
    config::maybe_show_log_hint()?;

    Ok(())
}

/// Display the append-only lifecycle audit for one submission, or every
/// distinct submission associated with a candidate change ID.
pub fn history(id: Option<&str>, change: Option<&str>, json: bool) -> Result<()> {
    require_initialized()?;
    let submissions = match (id, change) {
        (Some(id), None) => {
            let id = queue::parse_seq_id(id)?;
            let events = history::read(id)?;
            if events.is_empty() {
                return Err(ExitError::new(
                    exit_codes::USAGE,
                    format!("history for item {} not found", id),
                )
                .into());
            }
            vec![HistorySubmission {
                sequence_id: id,
                events,
            }]
        }
        (None, Some(change)) if !change.is_empty() => history::read_by_change(change)?
            .into_iter()
            .map(|(sequence_id, events)| HistorySubmission {
                sequence_id,
                events,
            })
            .collect(),
        (None, Some(_)) => {
            return Err(ExitError::new(exit_codes::USAGE, "change ID must not be empty").into());
        }
        (None, None) => {
            return Err(ExitError::new(
                exit_codes::USAGE,
                "usage: jjq history <ID> | jjq history --change <CHANGE_ID>",
            )
            .into());
        }
        (Some(_), Some(_)) => unreachable!("clap rejects conflicting history selectors"),
    };

    if submissions.is_empty() {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!(
                "no history found for candidate change ID '{}'",
                change.unwrap()
            ),
        )
        .into());
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&HistoryOutput {
                version: history::SCHEMA_VERSION,
                submissions,
            })?
        );
        return Ok(());
    }

    for (submission_index, submission) in submissions.iter().enumerate() {
        if submission_index > 0 {
            println!();
        }
        println!("History for item {}", submission.sequence_id);
        for event in &submission.events {
            let mut details = Vec::new();
            if let Some(change_id) = &event.candidate_change_id {
                details.push(format!("candidate {}", change_id));
            }
            if let Some(reason) = &event.failure_reason {
                details.push(format!("reason {}", reason));
            }
            if let Some(result) = &event.result_change_id {
                details.push(format!("result {}", result));
            }
            if let Some(elapsed_ms) = event.elapsed_ms {
                details.push(format!("{}ms", elapsed_ms));
            }
            if let Some(related) = event.related_sequence_id {
                details.push(format!("related item {}", related));
            }
            if let Some(paths) = &event.conflict_paths {
                details.push(format!("conflicts {}", paths.join(", ")));
            }
            if details.is_empty() {
                println!("  {} {}", event.timestamp, event.kind);
            } else {
                println!(
                    "  {} {} ({})",
                    event.timestamp,
                    event.kind,
                    details.join("; ")
                );
            }
        }
    }
    Ok(())
}

/// Move the invoking workspace's working copy off a just-queued commit.
///
/// A queued commit that is also the working-copy commit is amended by every
/// later snapshot in its workspace, and once it lands, trunk follows those
/// rewrites (jj bookmarks track rewritten commits) — silently absorbing
/// edits that were never queued or checked. Parking the working copy on a
/// fresh child (exactly what `jj commit` does) keeps the queued commit as
/// pushed. Only the invoking workspace can be moved: jj manipulates the
/// working copy of the workspace a command runs in, and no other.
/// Park the invoking workspace's working copy on a new empty child when it
/// still sits on the just-queued commit. Compare change IDs, not commit IDs:
/// jj rewrites the working-copy commit (and follows the queue bookmark) on
/// every snapshot, so any file change between the initial revset resolution
/// and this check makes the commit IDs diverge while the change stays the
/// same. A commit-ID comparison would then skip the move-off and later
/// landing would absorb the workspace's pending edits into trunk.
fn move_working_copy_off_queued(queued_change_id: &str) -> Result<()> {
    let (wc_change_id, _) = jj::resolve_revset_full("@")?;
    if wc_change_id == queued_change_id {
        jj::new_working_copy()?;
        prefout("working copy moved to a new change; the queued commit stays as pushed");
    }
    Ok(())
}

/// Configuration for one run invocation, read once per batch.
struct RunConfig {
    trunk_bookmark: String,
    check_command: String,
    strategy: Strategy,
    run_token: String,
}

/// Evidence that a successful check applies to one precise landing attempt.
///
/// Receipts deliberately stay in memory. They are only useful while the run
/// lock is held, and durable outcomes remain the bookmark state plus history.
/// The checked revision may contain workspace artifacts created by a check:
/// merge lands those artifacts, while rebase intentionally lands the original
/// candidate rebased onto trunk, as it did before receipts existed.
#[derive(Debug)]
struct ValidationReceipt {
    sequence_id: u32,
    candidate_change_id: String,
    candidate_commit_id: String,
    trunk_bookmark: String,
    trunk_commit_id: String,
    materialized_result_commit_id: String,
    checked_result_commit_id: String,
    strategy: Strategy,
    check_command: String,
    check_command_fingerprint: String,
}

impl ValidationReceipt {
    fn new(
        sequence_id: u32,
        candidate_change_id: String,
        candidate_commit_id: String,
        trunk_bookmark: String,
        trunk_commit_id: String,
        materialized_result_commit_id: String,
        checked_result_commit_id: String,
        strategy: Strategy,
        check_command: &str,
    ) -> Self {
        Self {
            sequence_id,
            candidate_change_id,
            candidate_commit_id,
            trunk_bookmark,
            trunk_commit_id,
            materialized_result_commit_id,
            checked_result_commit_id,
            strategy,
            check_command: check_command.to_string(),
            check_command_fingerprint: check_command_fingerprint(check_command),
        }
    }
}

/// Stable, dependency-free fingerprint for display and durable debugging of a
/// receipt's configured shell text. Receipt validation also compares the full
/// command string, so fingerprint collisions cannot weaken the safety check.
fn check_command_fingerprint(command: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001b3;
    let hash = command.as_bytes().iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    format!("fnv1a64:{hash:016x}")
}

/// Deterministic integration-test seam for the narrow window after receipt
/// validation. Normal invocations never set these variables. Keeping the
/// pause here proves metadata configuration writers cannot slip between the
/// fresh read and trunk CAS.
fn receipt_validated_test_hook() -> Result<()> {
    let Ok(started) = env::var("JJQTEST_RECEIPT_VALIDATED") else {
        return Ok(());
    };
    fs::write(&started, "")?;
    let Some(release) = env::var_os("JJQTEST_RECEIPT_VALIDATION_RELEASE") else {
        return Ok(());
    };
    let release = std::path::PathBuf::from(release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !release.exists() {
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for receipt validation test release");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

fn pause_test_hook(started_var: &str, release_var: &str) -> Result<()> {
    let Ok(started) = env::var(started_var) else {
        return Ok(());
    };
    fs::write(started, "")?;
    let Some(release) = env::var_os(release_var) else {
        return Ok(());
    };
    let release = PathBuf::from(release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !release.exists() {
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for {}", release_var);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

fn claim_published_test_hook() -> Result<()> {
    pause_test_hook("JJQTEST_CLAIM_PUBLISHED", "JJQTEST_CLAIM_RELEASE")
}

fn landing_cas_test_hook() -> Result<()> {
    pause_test_hook("JJQTEST_LANDING_CAS", "JJQTEST_LANDING_CAS_RELEASE")
}

fn queued_cancel_recorded_test_hook() -> Result<()> {
    pause_test_hook(
        "JJQTEST_QUEUED_CANCEL_RECORDED",
        "JJQTEST_QUEUED_CANCEL_RELEASE",
    )
}

fn running_cancel_marked_test_hook() -> Result<()> {
    pause_test_hook(
        "JJQTEST_RUNNING_CANCEL_MARKED",
        "JJQTEST_RUNNING_CANCEL_RELEASE",
    )
}

/// Reject a receipt if any fact that made its check meaningful changed.
///
/// This runs before a rebase mutates the original candidate or a bookmark CAS
/// moves trunk. A changed config is treated like a stale receipt even though
/// `run --all` otherwise reads config once: the new command/strategy needs a
/// fresh check before it can govern a landing.
fn validate_receipt(receipt: &ValidationReceipt, workspace_rev: &str) -> Result<()> {
    let snapshot = config::snapshot()?
        .ok_or_else(|| anyhow!("jjq metadata disappeared while validating receipt"))?;
    let current_strategy = snapshot.strategy()?;
    if current_strategy != receipt.strategy {
        bail!(
            "landing strategy changed from {} to {}",
            receipt.strategy.as_str(),
            current_strategy.as_str()
        );
    }
    let current_command = snapshot
        .check_command()?
        .ok_or_else(|| anyhow!("check_command was removed"))?;
    if current_command != receipt.check_command
        || check_command_fingerprint(&current_command) != receipt.check_command_fingerprint
    {
        bail!("check_command changed");
    }

    let current_trunk_bookmark = snapshot.trunk_bookmark()?;
    if current_trunk_bookmark != receipt.trunk_bookmark {
        bail!("trunk bookmark changed");
    }
    let current_trunk = jj::get_commit_id(&format!("bookmarks(exact:{})", current_trunk_bookmark))?;
    if current_trunk != receipt.trunk_commit_id {
        bail!("trunk moved");
    }

    let (current_change, current_commit) = jj::resolve_revset_full(&format!(
        "bookmarks(exact:{})",
        queue::running_bookmark(receipt.sequence_id)
    ))?;
    if current_change != receipt.candidate_change_id
        || current_commit != receipt.candidate_commit_id
    {
        bail!("queue claim no longer names the checked candidate");
    }

    let current_result = jj::get_commit_id(workspace_rev)?;
    if current_result != receipt.checked_result_commit_id {
        bail!("checked workspace revision changed");
    }

    Ok(())
}

/// Return a valid claim to its original queue position after a receipt was
/// invalidated. Never overwrite a retargeted claim: that is out-of-band state
/// which this runner did not check and must be left for explicit recovery.
fn requeue_invalid_receipt(
    receipt: &ValidationReceipt,
    allow_candidate_rewrite: bool,
) -> Result<bool> {
    let running = queue::running_bookmark(receipt.sequence_id);
    let queue_bookmark = queue::queue_bookmark(receipt.sequence_id);
    let Ok((current_change, current_commit)) =
        jj::resolve_revset_full(&format!("bookmarks(exact:{})", running))
    else {
        return Ok(false);
    };
    if current_change != receipt.candidate_change_id
        || (!allow_candidate_rewrite && current_commit != receipt.candidate_commit_id)
    {
        return Ok(false);
    }
    if !queue::queue_item_exists(receipt.sequence_id)? {
        jj::bookmark_create_internal(
            &queue_bookmark,
            &format!("bookmarks(exact:{})", running),
        )?;
    }
    jj::bookmark_delete(&running)?;
    Ok(true)
}

fn discard_receipt_work(
    receipt: &ValidationReceipt,
    strategy: Strategy,
    duplicate_ids: &[String],
    run_name: &str,
    reason: &str,
    allow_candidate_rewrite: bool,
) -> Result<()> {
    // Requeue before cleanup so a cleanup failure cannot strand a claim. The
    // rebase CAS path is the only caller allowed to see the original candidate
    // rewritten: validation has already bound its exact pre-rebase commit.
    let requeued = requeue_invalid_receipt(receipt, allow_candidate_rewrite)?;
    if strategy == config::Strategy::Rebase {
        for duplicate_id in duplicate_ids {
            let _ = jj::abandon(duplicate_id);
        }
    }
    jj::workspace_forget(run_name)?;
    if requeued {
        preferr(&format!(
            "validation receipt for item {} is stale ({}); item returned to queue",
            receipt.sequence_id, reason
        ));
    } else {
        preferr(&format!(
            "validation receipt for item {} is stale ({}); claim changed or was removed, discarding result",
            receipt.sequence_id, reason
        ));
    }
    Ok(())
}

/// Process queue items.
pub fn run(all: bool, stop_on_failure: bool, jobs: usize) -> Result<()> {
    // Pin once for both initialization and RunConfig. This replaces the
    // separate bookmark-existence probe without extending the snapshot past
    // the receipt boundary: validate_receipt still pins metadata again after
    // each successful check so concurrent config changes invalidate landing.
    let snapshot = config::snapshot()?.ok_or_else(|| {
        ExitError::new(
            exit_codes::USAGE,
            "jjq is not initialized. Run 'jjq init' first.",
        )
    })?;

    // Take the run lock after the same single metadata read that establishes
    // initialization, so a concurrent runner still exits promptly instead of
    // contending with the active one. Config file contents are loaded only
    // after the lock is acquired.
    let run_lock = match Lock::acquire("run")? {
        Some(lock) => lock,
        None => {
            return Err(
                ExitError::new(exit_codes::CONFLICT, "another queue runner is active").into(),
            );
        }
    };
    refuse_if_orphan_checks_live("starting a new run")?;

    // Read configuration once, right after taking the run lock, to materialize
    // each attempt consistently. A change while an item is checking invalidates
    // that receipt and stops the batch, so the next invocation reads the new
    // configuration. A missing check command is reported before touching any
    // queue item.
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
        run_token: format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ),
    };

    let (result, processed) = if all {
        if jobs > 1 && run_config.strategy == Strategy::Merge && !stop_on_failure {
            let outcome = run_all_parallel_merge(&run_lock, &run_config, jobs);
            batch_run_outcome(outcome)
        } else {
            if jobs > 1 {
                if stop_on_failure {
                    preferr(
                        "--jobs is serial with --stop-on-failure so no later checks start before an earlier terminal result",
                    );
                } else {
                    preferr(
                        "--jobs is currently serial for rebase: exact commit-bound receipts cannot safely reuse duplicate-based predictions",
                    );
                }
            }
            let outcome = run_all(&run_lock, &run_config, stop_on_failure);
            batch_run_outcome(outcome)
        }
    } else {
        match run_one(&run_lock, &run_config)? {
            RunResult::Success => (Ok(()), 1),
            RunResult::Empty => (Ok(()), 0),
            RunResult::Skipped => (Ok(()), 1),
            RunResult::Canceled => (Ok(()), 1),
            RunResult::StaleReceipt => (
                Err(ExitError::code_only(exit_codes::CONFLICT).into()),
                1,
            ),
            // run_one already reported the failure in detail
            RunResult::Failure => (Err(ExitError::code_only(exit_codes::CONFLICT).into()), 1),
        }
    };
    if processed > 0 {
        // Residue from this run's landings (a fixer's obsolete originals) is
        // abandoned while the run lock is still held. Failures are warnings.
        cleanup_residue()?;
    }
    result
}

/// Map a batch outcome of (processed count, any failure) onto the command
/// result and exit code: PARTIAL when some items failed, Ok otherwise.
fn batch_run_outcome(
    outcome: anyhow::Result<(usize, bool)>,
) -> (Result<()>, usize) {
    match outcome {
        Ok((count, failed)) if failed => (Err(ExitError::code_only(exit_codes::PARTIAL).into()), count),
        Ok((count, _)) => (Ok(()), count),
        Err(error) => (Err(error), 1),
    }
}

enum RunResult {
    Success,
    Empty,
    Skipped,
    Canceled,
    /// The checked facts changed. The unchanged claim was returned to the
    /// queue, so this batch must stop rather than immediately retry its stale
    /// configuration generation.
    StaleReceipt,
    /// The item failed (conflict or check failure). run_one reports the
    /// details; in single-run mode this maps to exit code CONFLICT.
    Failure,
}

fn run_all(
    run_lock: &Lock,
    run_config: &RunConfig,
    stop_on_failure: bool,
) -> Result<(usize, bool)> {
    let mut merged_count = 0u32;
    let mut failed_count = 0u32;
    let mut skipped_count = 0u32;
    let mut canceled_count = 0u32;
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
            RunResult::Canceled => canceled_count += 1,
            RunResult::StaleReceipt => {
                preferr(
                    "stopped after a stale validation receipt; run again to check the requeued item",
                );
                return Err(ExitError::code_only(exit_codes::CONFLICT).into());
            }
            RunResult::Failure => {
                if stop_on_failure {
                    if merged_count > 0 {
                        prefout(&format!(
                            "processed {} item(s) before failure",
                            merged_count
                        ));
                    }
                    return Ok((1, true));
                }
                failed_count += 1;
            }
        }
    }

    let processed = (merged_count + failed_count + skipped_count + canceled_count) as usize;
    if merged_count > 0 || failed_count > 0 || skipped_count > 0 || canceled_count > 0 {
        if failed_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} failed",
                merged_count, failed_count
            ));
            return Ok((processed, true));
        }
        if skipped_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} skipped (empty)",
                merged_count, skipped_count
            ));
        } else if canceled_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} canceled",
                merged_count, canceled_count
            ));
        } else {
            prefout(&format!("processed {} item(s)", merged_count));
        }
    }
    Ok((processed, false))
}

/// A claimed merge item materialized on a predicted trunk. The workspace and
/// claim live until the FIFO scheduler either consumes the exact receipt or
/// returns it to the queue for a fresh materialization.
struct SpeculativeAttempt {
    id: u32,
    candidate_change_id: String,
    candidate_commit_id: String,
    base_commit_id: String,
    workspace: Option<TempDir>,
    workspace_path: PathBuf,
    run_name: String,
    log_path: PathBuf,
    run_token: String,
    outcome: SpeculativeOutcome,
}

enum SpeculativeOutcome {
    Ready {
        materialized_result_commit_id: String,
        check_status: Option<Result<crate::runner::BackgroundCheckResult, String>>,
        elapsed_ms: Option<u128>,
    },
    Conflict(Vec<String>),
    Empty,
    /// Refused by the run-time landing-policy re-check before any
    /// materialization; the item was already moved to failed.
    PolicyRejected,
}

/// Merge-only bounded optimistic lookahead. Rebase remains deliberately
/// serial: the current materialization is a duplicate with a different change
/// ID, so it cannot produce a receipt whose exact base is the later rebase of
/// the original candidate without mutating that candidate before its check.
fn run_all_parallel_merge(
    run_lock: &Lock,
    run_config: &RunConfig,
    jobs: usize,
) -> Result<(usize, bool)> {
    let run_token = run_config.run_token.clone();
    let mut merged_count = 0u32;
    let mut failed_count = 0u32;
    let mut skipped_count = 0u32;
    let mut canceled_count = 0u32;

    loop {
        // Recover once per scheduling window, before claims are made. This is
        // the same crash recovery as run_one and preserves FIFO by ID.
        for stale_id in queue::get_running()? {
            cleanup_run_workspace(stale_id);
            if recover_claim(stale_id)? {
                prefout(&format!(
                    "completed cancellation of interrupted item {}",
                    stale_id
                ));
            } else {
                prefout(&format!("recovered interrupted item {}", stale_id));
            }
        }
        // A prior runner's records were retained through stale workspace
        // recovery so a surviving check child was not disrupted. They cannot
        // represent this new run token and are now only stale observability.
        prune_dead_attempt_state();

        let mut predicted_base =
            jj::get_commit_id(&format!("bookmarks(exact:{})", run_config.trunk_bookmark))?;
        let mut attempts = Vec::new();
        for _ in 0..jobs {
            let Some(attempt) = prepare_speculative_merge(&predicted_base, &run_token)? else {
                break;
            };
            if let SpeculativeOutcome::Ready {
                materialized_result_commit_id,
                ..
            } = &attempt.outcome
            {
                predicted_base = materialized_result_commit_id.clone();
            }
            attempts.push(attempt);
            // A predicted conflict has no meaningful descendant base. Its
            // terminality still depends on earlier receipts, so defer it to
            // FIFO consumption rather than marking it failed here.
            if !matches!(
                attempts.last().unwrap().outcome,
                SpeculativeOutcome::Ready { .. }
            ) {
                break;
            }
        }

        if attempts.is_empty() {
            break;
        }
        start_speculative_checks(&mut attempts, run_config)?;

        let mut rescheduled = false;
        for index in 0..attempts.len() {
            // A predecessor can have failed or produced check artifacts. In
            // either case this predicted base is no longer exact, so discard
            // rather than emitting a false failure or reusing its receipt.
            let current_trunk =
                jj::get_commit_id(&format!("bookmarks(exact:{})", run_config.trunk_bookmark))?;
            if current_trunk != attempts[index].base_commit_id {
                let _publication_lock = Lock::acquire_blocking("publication")?;
                discard_speculative_attempt(&mut attempts[index])?;
                for later in attempts.iter_mut().skip(index + 1) {
                    discard_speculative_attempt(later)?;
                }
                rescheduled = true;
                break;
            }

            match consume_speculative_merge(&mut attempts[index], run_config)? {
                RunResult::Success => merged_count += 1,
                RunResult::Skipped => skipped_count += 1,
                RunResult::Canceled => {
                    canceled_count += 1;
                    let _publication_lock = Lock::acquire_blocking("publication")?;
                    for later in attempts.iter_mut().skip(index + 1) {
                        discard_speculative_attempt(later)?;
                    }
                    rescheduled = true;
                    break;
                }
                RunResult::Failure => {
                    failed_count += 1;
                    let _publication_lock = Lock::acquire_blocking("publication")?;
                    for later in attempts.iter_mut().skip(index + 1) {
                        discard_speculative_attempt(later)?;
                    }
                    rescheduled = true;
                    break;
                }
                RunResult::StaleReceipt => {
                    let _publication_lock = Lock::acquire_blocking("publication")?;
                    for later in attempts.iter_mut().skip(index + 1) {
                        discard_speculative_attempt(later)?;
                    }
                    preferr(
                        "stopped after a stale validation receipt; run again to check the requeued item",
                    );
                    return Err(ExitError::code_only(exit_codes::CONFLICT).into());
                }
                RunResult::Empty => unreachable!("claimed speculative item cannot be empty queue"),
            }
        }
        if rescheduled {
            continue;
        }
    }

    if merged_count > 0 || failed_count > 0 || skipped_count > 0 || canceled_count > 0 {
        if failed_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} failed",
                merged_count, failed_count
            ));
        } else if skipped_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} skipped (empty)",
                merged_count, skipped_count
            ));
        } else if canceled_count > 0 {
            prefout(&format!(
                "processed {} item(s), {} canceled",
                merged_count, canceled_count
            ));
        } else {
            prefout(&format!("processed {} item(s)", merged_count));
        }
    }
    let _ = run_lock; // documents that this scheduler owns the exclusive run.
    Ok((
        (merged_count + failed_count + skipped_count + canceled_count) as usize,
        failed_count > 0,
    ))
}

fn prepare_speculative_merge(
    base_commit_id: &str,
    run_token: &str,
) -> Result<Option<SpeculativeAttempt>> {
    // Allocate the path before entering publication.lock so batching its
    // recovery metadata with the claim event does not delay claim publication.
    let workspace = TempDir::new()?;
    let workspace_path = workspace.path().to_path_buf();
    let publication_lock = Lock::acquire_blocking("publication")?;
    let id = loop {
        let Some(id) = queue::next_item()? else {
            return Ok(None);
        };
        if cancellation_requested(id) {
            terminalize_queued_cancellation(id)?;
            continue;
        }
        break id;
    };
    prefout(&format!("processing queue item {} (merge strategy)", id));
    let running = queue::running_bookmark(id);
    jj::bookmark_rename(&queue::queue_bookmark(id), &running)?;
    let (candidate_change_id, candidate_commit_id) =
        jj::resolve_revset_full(&format!("bookmarks(exact:{})", running))?;
    let mut claimed = Event::new(id, kind::CLAIMED);
    claimed.candidate_change_id = Some(candidate_change_id.clone());
    claimed.candidate_commit_id = Some(candidate_commit_id.clone());
    claimed.trunk_commit_id = Some(base_commit_id.to_string());
    claimed.strategy = Some(Strategy::Merge.as_str().to_string());
    let run_name = format!("jjq-run-{}-{}", queue::format_seq_id(id), run_token);
    history::record_claim_with_workspace(claimed, workspace_path.to_str().unwrap(), &run_name)?;
    drop(publication_lock);
    claim_published_test_hook()?;

    // Authoritative landing-policy re-check for the speculative path, same
    // as the serial one. A violation is a terminal failure for the item.
    if let Err(policy_error) = check_landing_policy(
        &candidate_commit_id,
        base_commit_id,
        history::allow_stack(id)?,
    ) {
        let _publication_lock = Lock::acquire_blocking("publication")?;
        let trunk_bookmark = config::get_trunk_bookmark()?;
        let trunk_commit_id =
            jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;
        reject_claimed_item(
            id,
            &candidate_change_id,
            &candidate_commit_id,
            &trunk_commit_id,
            &workspace_path,
            &Strategy::Merge,
            &policy_error.to_string(),
        )?;
        // Return an attempt whose outcome reports the rejection as a
        // failure for FIFO consumption.
        let attempt = SpeculativeAttempt {
            id,
            candidate_change_id,
            candidate_commit_id,
            base_commit_id: base_commit_id.to_string(),
            workspace: Some(workspace),
            workspace_path,
            run_name,
            log_path: crate::runlog::attempt_log(run_token, id)?,
            run_token: run_token.to_string(),
            outcome: SpeculativeOutcome::PolicyRejected,
        };
        return Ok(Some(attempt));
    }

    jj::workspace_add(
        workspace_path.to_str().unwrap(),
        &run_name,
        &[base_commit_id, &format!("bookmarks(exact:{})", running)],
    )?;
    let workspace_rev = format!("{}@", run_name);
    let outcome = if jj::has_conflicts(&workspace_rev)? {
        SpeculativeOutcome::Conflict(jj::conflict_paths(&workspace_rev)?)
    } else if jj::trees_match(base_commit_id, &workspace_rev)? {
        SpeculativeOutcome::Empty
    } else {
        // This is the final merge description. A later item can safely name
        // this commit as its predicted base unless the check snapshots edits.
        jj::describe(&workspace_rev, &format!("Success: merge {}", id))?;
        SpeculativeOutcome::Ready {
            materialized_result_commit_id: jj::get_commit_id(&workspace_rev)?,
            check_status: None,
            elapsed_ms: None,
        }
    };
    Ok(Some(SpeculativeAttempt {
        id,
        candidate_change_id,
        candidate_commit_id,
        base_commit_id: base_commit_id.to_string(),
        workspace: Some(workspace),
        workspace_path,
        run_name,
        log_path: crate::runlog::attempt_log(run_token, id)?,
        run_token: run_token.to_string(),
        outcome,
    }))
}

fn start_speculative_checks(
    attempts: &mut [SpeculativeAttempt],
    run_config: &RunConfig,
) -> Result<()> {
    let mut handles = Vec::new();
    for (index, attempt) in attempts.iter_mut().enumerate() {
        if !matches!(attempt.outcome, SpeculativeOutcome::Ready { .. }) {
            continue;
        }
        let mut event = Event::new(attempt.id, kind::CHECK_STARTED);
        event.candidate_change_id = Some(attempt.candidate_change_id.clone());
        event.candidate_commit_id = Some(attempt.candidate_commit_id.clone());
        event.trunk_commit_id = Some(attempt.base_commit_id.clone());
        event.strategy = Some(Strategy::Merge.as_str().to_string());
        record_history(event)?;
        let command = run_config.check_command.clone();
        let path = attempt.workspace_path.clone();
        let log_path = attempt.log_path.clone();
        let process_identity = format!("jjq-check:{}:{}", attempt.run_token, attempt.id);
        let start_gate = paths::jjq_dir()?
            .join("start-gates")
            .join(&attempt.run_token)
            .join(queue::format_seq_id(attempt.id));
        let cancel_marker = cancellation_path(attempt.id)?;
        let gate_for_publish = start_gate.clone();
        let started_at_ms = unix_time_ms();
        let active_check = ActiveCheck {
            id: attempt.id,
            run_token: attempt.run_token.clone(),
            predicted_base: attempt.base_commit_id.clone(),
            speculative: index > 0,
            workspace_path: attempt.workspace_path.display().to_string(),
            log_path: attempt.log_path.display().to_string(),
            phase: "checking".to_string(),
            process_start: None,
            pid: None,
            published_at_ms: started_at_ms,
            started_at_ms,
            elapsed_ms: 0,
            gate_opened: false,
        };
        publish_active_check(&active_check)?;
        handles.push((
            index,
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let status = crate::runner::run_check_command_noninteractive(
                    &command,
                    &path,
                    &log_path,
                    &start_gate,
                    &cancel_marker,
                    std::process::id(),
                    &process_identity,
                    move |pid| {
                        let _publication_lock = Lock::acquire_blocking("publication")?;
                        if cancellation_requested(active_check.id) {
                            return Ok(true);
                        }
                        let mut running_check = active_check;
                        running_check.pid = Some(pid);
                        running_check.process_start = Some(process_start_identity(pid)?);
                        running_check.published_at_ms = unix_time_ms();
                        publish_active_check(&running_check)?;
                        if let Some(parent) = gate_for_publish.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        let gate = fs::File::create(&gate_for_publish)?;
                        gate.sync_all()?;
                        running_check.gate_opened = true;
                        publish_active_check(&running_check)?;
                        Ok(false)
                    },
                )
                .map_err(|error| error.to_string());
                (status, start.elapsed().as_millis())
            }),
        ));
    }
    for (index, handle) in handles {
        let (status, elapsed_ms) = handle
            .join()
            .map_err(|_| anyhow!("speculative check thread panicked"))?;
        if let SpeculativeOutcome::Ready {
            check_status,
            elapsed_ms: recorded_elapsed,
            ..
        } = &mut attempts[index].outcome
        {
            *check_status = Some(status);
            *recorded_elapsed = Some(elapsed_ms);
        }
    }
    Ok(())
}

/// Return a speculative claim to its original FIFO position. No terminal
/// event is written: a prediction that never matched trunk was never an
/// actual merge attempt.
fn discard_speculative_attempt(attempt: &mut SpeculativeAttempt) -> Result<()> {
    if cancellation_requested(attempt.id) {
        let _ = cancel_speculative_attempt(attempt, 0)?;
        return Ok(());
    }
    let running = queue::running_bookmark(attempt.id);
    if jj::bookmark_exists(&running)? {
        if !queue::queue_item_exists(attempt.id)? {
            jj::bookmark_create_internal(
                &queue::queue_bookmark(attempt.id),
                &format!("bookmarks(exact:{})", running),
            )?;
        }
        jj::bookmark_delete(&running)?;
    }
    jj::workspace_forget(&attempt.run_name)?;
    attempt.workspace.take();
    remove_attempt_log(&attempt.log_path);
    remove_active_check(&attempt.run_token, attempt.id);
    Ok(())
}

/// Terminalize a speculative cancellation while publication.lock is held.
fn cancel_speculative_attempt(
    attempt: &mut SpeculativeAttempt,
    elapsed_ms: u128,
) -> Result<RunResult> {
    let running = queue::running_bookmark(attempt.id);
    if jj::bookmark_exists(&running)? {
        // Record first: recovery can idempotently finish deleting a claim if
        // the process stops between these two durable mutations.
        record_cancellation_terminal(
            attempt.id,
            &attempt.candidate_change_id,
            &attempt.candidate_commit_id,
            Some(&attempt.base_commit_id),
            Some(Strategy::Merge),
            Some(elapsed_ms),
        )?;
        jj::bookmark_delete(&running)?;
    }
    jj::workspace_forget(&attempt.run_name)?;
    attempt.workspace.take();
    remove_active_check(&attempt.run_token, attempt.id);
    remove_attempt_log(&attempt.log_path);
    remove_cancellation(attempt.id);
    prefout(&format!("canceled queue item {}", attempt.id));
    Ok(RunResult::Canceled)
}

fn consume_speculative_merge(
    attempt: &mut SpeculativeAttempt,
    run_config: &RunConfig,
) -> Result<RunResult> {
    // Cancellation intent and every terminal choice are serialized here.
    // `cancel` takes mutation -> publication; the runner already owns run and
    // takes publication -> meta, matching the documented lock order.
    let _publication_lock = Lock::acquire_blocking("publication")?;
    if cancellation_requested(attempt.id) {
        return cancel_speculative_attempt(attempt, 0);
    }
    let workspace_rev = format!("{}@", attempt.run_name);
    match &mut attempt.outcome {
        SpeculativeOutcome::Conflict(conflicts) => {
            let context = FailureContext {
                candidate_change_id: &attempt.candidate_change_id,
                candidate_commit_id: &attempt.candidate_commit_id,
                trunk_commit_id: &attempt.base_commit_id,
                workspace_path: &attempt.workspace_path,
                strategy: &Strategy::Merge,
            };
            jj::bookmark_create_internal(
                &queue::failed_bookmark(attempt.id),
                &workspace_rev,
            )?;
            jj::bookmark_delete(&queue::running_bookmark(attempt.id))?;
            jj::describe(
                &workspace_rev,
                &failure_description(attempt.id, "conflicts", &context, conflicts),
            )?;
            let mut event = Event::new(attempt.id, kind::FAILED);
            event.candidate_change_id = Some(attempt.candidate_change_id.clone());
            event.candidate_commit_id = Some(attempt.candidate_commit_id.clone());
            event.trunk_commit_id = Some(attempt.base_commit_id.clone());
            event.strategy = Some(Strategy::Merge.as_str().to_string());
            event.failure_reason = Some("conflicts".to_string());
            event.conflict_paths = (!conflicts.is_empty()).then_some(conflicts.clone());
            record_history(event)?;
            if let Some(workspace) = attempt.workspace.take() {
                let _ = workspace.keep();
            }
            remove_active_check(&attempt.run_token, attempt.id);
            preferr(&format!(
                "merge {} has conflicts, marked as failed",
                attempt.id
            ));
            preferr(&format!("  candidate: {}", attempt.candidate_change_id));
            if !conflicts.is_empty() {
                preferr(&format!("  conflicting files: {}", conflicts.join(", ")));
            }
            return Ok(RunResult::Failure);
        }
        SpeculativeOutcome::PolicyRejected => {
            // The rejection happens before workspace_add, so run_name was
            // never registered — forgetting it would fail the whole run.
            attempt.workspace.take();
            remove_active_check(&attempt.run_token, attempt.id);
            Ok(RunResult::Failure)
        }
        SpeculativeOutcome::Empty => {
            jj::bookmark_delete(&queue::running_bookmark(attempt.id))?;
            let mut event = Event::new(attempt.id, kind::SKIPPED_EMPTY);
            event.candidate_change_id = Some(attempt.candidate_change_id.clone());
            event.candidate_commit_id = Some(attempt.candidate_commit_id.clone());
            event.trunk_commit_id = Some(attempt.base_commit_id.clone());
            event.strategy = Some(Strategy::Merge.as_str().to_string());
            record_history(event)?;
            jj::workspace_forget(&attempt.run_name)?;
            attempt.workspace.take();
            remove_active_check(&attempt.run_token, attempt.id);
            preferr(&format!(
                "queue item {} is empty (no changes vs {}), skipping",
                attempt.id, run_config.trunk_bookmark
            ));
            return Ok(RunResult::Skipped);
        }
        SpeculativeOutcome::Ready {
            materialized_result_commit_id,
            check_status,
            elapsed_ms,
        } => {
            let status = check_status
                .take()
                .ok_or_else(|| anyhow!("speculative check was not started"))?
                .map_err(|error| anyhow!("speculative check failed to run: {}", error))?;
            let elapsed_ms = elapsed_ms.unwrap_or_default();
            jj::snapshot_workspace(&attempt.workspace_path)?;
            crate::runlog::publish_attempt_log(&attempt.log_path)?;
            if status.cancelled || cancellation_requested(attempt.id) {
                return cancel_speculative_attempt(attempt, elapsed_ms);
            }
            if !status.status.success() {
                if let Ok(log_contents) = fs::read_to_string(&attempt.log_path) {
                    for line in log_contents.lines() {
                        if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                            eprintln!("{}", line);
                        }
                    }
                }
                crate::runlog::archive_failed(attempt.id, &attempt.log_path)?;
                remove_attempt_log(&attempt.log_path);
                let context = FailureContext {
                    candidate_change_id: &attempt.candidate_change_id,
                    candidate_commit_id: &attempt.candidate_commit_id,
                    trunk_commit_id: &attempt.base_commit_id,
                    workspace_path: &attempt.workspace_path,
                    strategy: &Strategy::Merge,
                };
                jj::bookmark_create_internal(
                    &queue::failed_bookmark(attempt.id),
                    &workspace_rev,
                )?;
                jj::bookmark_delete(&queue::running_bookmark(attempt.id))?;
                jj::describe(
                    &workspace_rev,
                    &failure_description(attempt.id, "check", &context, &[]),
                )?;
                let mut event = Event::new(attempt.id, kind::FAILED);
                event.candidate_change_id = Some(attempt.candidate_change_id.clone());
                event.candidate_commit_id = Some(attempt.candidate_commit_id.clone());
                event.trunk_commit_id = Some(attempt.base_commit_id.clone());
                event.strategy = Some(Strategy::Merge.as_str().to_string());
                event.failure_reason = Some("check".to_string());
                event.elapsed_ms = Some(elapsed_ms);
                record_history(event)?;
                if let Some(workspace) = attempt.workspace.take() {
                    let _ = workspace.keep();
                }
                remove_active_check(&attempt.run_token, attempt.id);
                preferr(&format!(
                    "merge {} failed check, marked as failed",
                    attempt.id
                ));
                return Ok(RunResult::Failure);
            }

            let receipt = ValidationReceipt::new(
                attempt.id,
                attempt.candidate_change_id.clone(),
                attempt.candidate_commit_id.clone(),
                run_config.trunk_bookmark.clone(),
                attempt.base_commit_id.clone(),
                materialized_result_commit_id.clone(),
                jj::get_commit_id(&workspace_rev)?,
                Strategy::Merge,
                &run_config.check_command,
            );
            let meta_lock = meta::acquire()?;
            if let Err(error) = validate_receipt(&receipt, &workspace_rev) {
                drop(meta_lock);
                discard_speculative_attempt(attempt)?;
                preferr(&format!(
                    "validation receipt for item {} is stale ({}); item returned to queue",
                    attempt.id, error
                ));
                return Ok(RunResult::StaleReceipt);
            }
            let landed_change_id = jj::resolve_revset(&receipt.checked_result_commit_id)?;
            if let Err(error) = jj::bookmark_move(
                &receipt.trunk_bookmark,
                &receipt.trunk_commit_id,
                &receipt.checked_result_commit_id,
            ) {
                drop(meta_lock);
                discard_speculative_attempt(attempt)?;
                preferr(&format!(
                    "validation receipt for item {} is stale (trunk changed while landing: {}); item returned to queue",
                    attempt.id, error
                ));
                return Ok(RunResult::StaleReceipt);
            }
            landing_cas_test_hook()?;
            jj::bookmark_delete(&queue::running_bookmark(attempt.id))?;
            jj::workspace_forget(&attempt.run_name)?;
            attempt.workspace.take();
            drop(meta_lock);
            let (result_change_id, result_commit_id) =
                jj::resolve_revset_full(&format!("change_id({})", landed_change_id))?;
            let mut event = Event::new(attempt.id, kind::LANDED);
            event.candidate_change_id = Some(attempt.candidate_change_id.clone());
            event.candidate_commit_id = Some(attempt.candidate_commit_id.clone());
            event.trunk_commit_id = Some(attempt.base_commit_id.clone());
            event.result_change_id = Some(result_change_id);
            event.result_commit_id = Some(result_commit_id);
            event.strategy = Some(Strategy::Merge.as_str().to_string());
            event.elapsed_ms = Some(elapsed_ms);
            record_history(event)?;
            remove_attempt_log(&attempt.log_path);
            remove_active_check(&attempt.run_token, attempt.id);
            prefout(&format!(
                "merged {} to {} (now at {})",
                attempt.id, receipt.trunk_bookmark, landed_change_id
            ));
            return Ok(RunResult::Success);
        }
    }
}

/// Move a stale running claim back to its queue position (same ID). Callers
/// must know the claim is orphaned: either they hold the run lock, or they
/// verified the run lock is free.
fn recover_claim(id: u32) -> Result<bool> {
    let _publication_lock = Lock::acquire_blocking("publication")?;
    let running_bookmark = queue::running_bookmark(id);
    if cancellation_requested(id) {
        let (candidate_change_id, candidate_commit_id) =
            jj::resolve_revset_full(&format!("bookmarks(exact:{})", running_bookmark))?;
        let trunk_commit_id = config::get_trunk_bookmark()
            .ok()
            .and_then(|trunk| jj::get_commit_id(&format!("bookmarks(exact:{})", trunk)).ok());
        record_cancellation_terminal(
            id,
            &candidate_change_id,
            &candidate_commit_id,
            trunk_commit_id.as_deref(),
            None,
            None,
        )?;
        jj::bookmark_delete(&running_bookmark)?;
        remove_cancellation(id);
        return Ok(true);
    }
    let event = history_for_revision(
        id,
        kind::RECOVERED,
        &format!("bookmarks(exact:{})", running_bookmark),
    )?;
    let queue_bookmark = queue::queue_bookmark(id);
    if queue::queue_item_exists(id)? {
        // Preserve recovery of the legacy crash shape where queue and running
        // both exist because the old create-then-delete transition stopped
        // between commands.
        jj::bookmark_delete(&running_bookmark)?;
    } else {
        jj::bookmark_rename(&running_bookmark, &queue_bookmark)?;
    }
    record_history(event)?;
    Ok(false)
}

/// Reject a claimed item for a landing-policy violation, using the same
/// create-before-delete bookkeeping as the run failure paths. The caller
/// holds publication.lock. Returns the failed marker commit's change ID.
#[allow(clippy::too_many_arguments)]
fn reject_claimed_item(
    id: u32,
    candidate_change_id: &str,
    candidate_commit_id: &str,
    trunk_commit_id: &str,
    _workspace_path: &std::path::Path,
    strategy: &Strategy,
    reason: &str,
) -> Result<()> {
    let marker = jj::new_rev(&[candidate_commit_id])?;
    let entry = queue::StateEntry {
        id,
        change_id: candidate_change_id.to_string(),
        commit_id: candidate_commit_id.to_string(),
        description: String::new(),
    };
    let desc = reject_description(id, &entry, trunk_commit_id, strategy, reason);
    jj::describe(&marker, &desc)?;
    jj::bookmark_create_internal(&queue::failed_bookmark(id), &marker)?;
    jj::bookmark_delete(&queue::running_bookmark(id))?;
    let mut event = Event::new(id, kind::REJECTED);
    event.candidate_change_id = Some(candidate_change_id.to_string());
    event.candidate_commit_id = Some(candidate_commit_id.to_string());
    event.trunk_commit_id = Some(trunk_commit_id.to_string());
    event.strategy = Some(strategy.as_str().to_string());
    event.failure_reason = Some(reason.to_string());
    record_history(event)?;
    preferr(&format!("merge {} rejected: {}", id, reason));
    Ok(())
}

/// A visible leaf commit considered for residue cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leaf {
    change_id: String,
    commit_id: String,
    description: String,
}

/// Partition leaves into (abandon, keep). A leaf is abandoned only when its
/// exact commit ID was recorded as the queued candidate of a submission that
/// landed; every other leaf is kept. Commit-ID matching, not change-ID: a
/// divergent copy of a landed change can carry unlanded local edits, and a
/// change-ID match would abandon them.
fn residue_selection(
    leaves: &[Leaf],
    landed_candidates: &std::collections::HashSet<String>,
) -> (Vec<Leaf>, Vec<Leaf>) {
    let mut abandon: Vec<Leaf> = Vec::new();
    let mut keep: Vec<Leaf> = Vec::new();
    for leaf in leaves {
        let leaf = leaf.clone();
        if landed_candidates.contains(&leaf.commit_id) {
            abandon.push(leaf);
        } else {
            keep.push(leaf);
        }
    }
    (abandon, keep)
}

/// Abandon residue leaves after a run processed items or before clean
/// finishes. Failures are downgraded to warnings: cleanup must never fail
/// the caller's run.
fn cleanup_residue() -> Result<()> {
    let result = cleanup_residue_inner();
    if let Err(error) = &result {
        preferr(&format!("residue cleanup failed: {}", error));
    }
    Ok(())
}

fn cleanup_residue_inner() -> Result<()> {
    let trunk = config::get_trunk_bookmark()?;
    let revset = format!(
        r#"heads(~::bookmarks(exact:{})) ~ bookmarks() ~ working_copies() ~ ({})"#,
        trunk,
        meta::meta_revset()
    );
    // Unit-separator framing: change_id \x1f commit_id \x1f json-escaped
    // first description line, then \x1e record end. escape_json makes the
    // description safe to split on.
    let template = concat!(
        r#"change_id.short() ++ "\x1f" ++ commit_id ++ "\x1f" ++"#,
        r#"description.first_line().escape_json() ++ "\x1e" ++ "\n""#,
    );
    let output = jj::run_ok_ignore_working_copy(&[
        "log",
        "-r",
        &revset,
        "--no-graph",
        "-T",
        template,
    ])?;

    let mut leaves: Vec<Leaf> = Vec::new();
    for record in output.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let mut parts = record.split('\u{1f}');
        let (Some(change_id), Some(commit_id), Some(description)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let description = serde_json::from_str::<String>(description)
            .unwrap_or_else(|_| description.trim().to_string());
        leaves.push(Leaf {
            change_id: change_id.to_string(),
            commit_id: commit_id.to_string(),
            description,
        });
    }

    let landed_candidates = history::landed_candidate_commit_ids()?;
    let (abandon, keep) = residue_selection(&leaves, &landed_candidates);
    for leaf in &abandon {
        jj::abandon(&leaf.commit_id)?;
        prefout(&format!(
            "residue: abandoned {} {}",
            leaf.change_id, leaf.description
        ));
    }
    for leaf in &keep {
        prefout(&format!(
            "residue: kept {} (not a queued candidate of landed work)",
            leaf.change_id
        ));
    }
    Ok(())
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
        if recover_claim(stale_id)? {
            prefout(&format!(
                "completed cancellation of interrupted item {}",
                stale_id
            ));
        } else {
            prefout(&format!("recovered interrupted item {}", stale_id));
        }
    }
    prune_dead_attempt_state();

    // Allocate the path before entering publication.lock so batching its
    // recovery metadata with the claim event does not delay claim publication.
    let runner_workspace = TempDir::new()?;
    let runner_workspace_path = runner_workspace.path().to_path_buf();

    // Keep publication only around the queue -> running transition and its
    // matching history/recovery-metadata transaction. Workspace setup and
    // checks do not need it.
    let publication_lock = Lock::acquire_blocking("publication")?;
    let id = match queue::next_item()? {
        Some(id) => id,
        None => {
            drop(publication_lock);
            prefout("queue is empty");
            return Ok(RunResult::Empty);
        }
    };

    if cancellation_requested(id) {
        terminalize_queued_cancellation(id)?;
        return Ok(RunResult::Canceled);
    }

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
    jj::bookmark_rename(&queue::queue_bookmark(id), &running_bookmark)?;

    // Record trunk commit ID
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;

    // Capture candidate change ID before creating workspace
    let (candidate_change_id, candidate_commit_id) =
        jj::resolve_revset_full(&format!("bookmarks(exact:{})", running_bookmark))?;
    let mut claimed = Event::new(id, kind::CLAIMED);
    claimed.candidate_change_id = Some(candidate_change_id.clone());
    claimed.candidate_commit_id = Some(candidate_commit_id.clone());
    claimed.trunk_commit_id = Some(trunk_commit_id.clone());
    claimed.strategy = Some(strategy.as_str().to_string());
    let run_name = format!(
        "jjq-run-{}-{}",
        queue::format_seq_id(id),
        run_config.run_token
    );
    history::record_claim_with_workspace(
        claimed,
        runner_workspace_path.to_str().unwrap(),
        &run_name,
    )?;
    drop(publication_lock);
    claim_published_test_hook()?;

    // Authoritative landing-policy re-check. The push-time gate is
    // synchronous feedback; this is the gate that governs landing.
    if let Err(policy_error) = check_landing_policy(
        &candidate_commit_id,
        &trunk_commit_id,
        history::allow_stack(id)?,
    ) {
        let _publication_lock = Lock::acquire_blocking("publication")?;
        reject_claimed_item(
            id,
            &candidate_change_id,
            &candidate_commit_id,
            &trunk_commit_id,
            &runner_workspace_path,
            &strategy,
            &policy_error.to_string(),
        )?;
        return Ok(RunResult::Failure);
    }

    // Token-scoped until FIFO terminal consumption. This is the same attempt
    // isolation used by speculative workers.
    let log_path = crate::runlog::attempt_log(&run_config.run_token, id)?;

    // Capture original description for rebase success path (before the claim is deleted)
    let candidate_description =
        jj::get_description(&format!("bookmarks(exact:{})", running_bookmark)).unwrap_or_default();

    // Create workspace — strategy determines how
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

    let workspace_path = runner_workspace_path;
    let failure_context = FailureContext {
        candidate_change_id: &candidate_change_id,
        candidate_commit_id: &candidate_commit_id,
        trunk_commit_id: &trunk_commit_id,
        workspace_path: &workspace_path,
        strategy: &strategy,
    };

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
        let _publication_lock = Lock::acquire_blocking("publication")?;
        if cancellation_requested(id) {
            cancel_serial_attempt(
                id,
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                strategy,
                &run_name,
                &run_config.run_token,
                &log_path,
                &rebase_duplicate_ids,
                0,
            )?;
            return Ok(RunResult::Canceled);
        }
        // Create the failed entry before deleting the claim so a crash
        // between the two duplicates the item instead of losing it.
        jj::bookmark_create_internal(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::bookmark_delete(&running_bookmark)?;
        jj::describe(
            &workspace_rev,
            &failure_description(id, "conflicts", &failure_context, &conflicts),
        )?;
        let mut event = Event::new(id, kind::FAILED);
        event.candidate_change_id = Some(candidate_change_id.clone());
        event.candidate_commit_id = Some(candidate_commit_id.clone());
        event.trunk_commit_id = Some(trunk_commit_id.clone());
        event.strategy = Some(strategy.as_str().to_string());
        event.failure_reason = Some("conflicts".to_string());
        event.conflict_paths = (!conflicts.is_empty()).then_some(conflicts.clone());
        record_history(event)?;

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
        preferr(&format!("  # resolve conflicts in {}", candidate_change_id));
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
        let _publication_lock = Lock::acquire_blocking("publication")?;
        if cancellation_requested(id) {
            cancel_serial_attempt(
                id,
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                strategy,
                &run_name,
                &run_config.run_token,
                &log_path,
                &rebase_duplicate_ids,
                0,
            )?;
            return Ok(RunResult::Canceled);
        }
        jj::bookmark_delete(&running_bookmark)?;
        let mut event = Event::new(id, kind::SKIPPED_EMPTY);
        event.candidate_change_id = Some(candidate_change_id.clone());
        event.candidate_commit_id = Some(candidate_commit_id.clone());
        event.trunk_commit_id = Some(trunk_commit_id.clone());
        event.strategy = Some(strategy.as_str().to_string());
        record_history(event)?;

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
    let materialized_result_commit_id = jj::get_commit_id(&workspace_rev)?;

    // Run check command with the runner workspace as its working directory
    let mut check_started = Event::new(id, kind::CHECK_STARTED);
    check_started.candidate_change_id = Some(candidate_change_id.clone());
    check_started.candidate_commit_id = Some(candidate_commit_id.clone());
    check_started.trunk_commit_id = Some(trunk_commit_id.clone());
    check_started.strategy = Some(strategy.as_str().to_string());
    record_history(check_started)?;
    let start_gate = paths::jjq_dir()?
        .join("start-gates")
        .join(&run_config.run_token)
        .join(queue::format_seq_id(id));
    let cancel_marker = cancellation_path(id)?;
    let started_at_ms = unix_time_ms();
    let active_check = ActiveCheck {
        id,
        run_token: run_config.run_token.clone(),
        predicted_base: trunk_commit_id.clone(),
        speculative: false,
        workspace_path: workspace_path.display().to_string(),
        log_path: log_path.display().to_string(),
        phase: "checking".to_string(),
        process_start: None,
        pid: None,
        published_at_ms: started_at_ms,
        started_at_ms,
        elapsed_ms: 0,
        gate_opened: false,
    };
    publish_active_check(&active_check)?;
    let check_started_at = std::time::Instant::now();
    let gate_for_publish = start_gate.clone();
    let check_status = crate::runner::run_check_command_cancellable(
        &run_config.check_command,
        &workspace_path,
        &log_path,
        &start_gate,
        &cancel_marker,
        std::process::id(),
        &format!("jjq-check:{}:{}", run_config.run_token, id),
        move |pid| {
            // Publish identity and open the command gate while holding the
            // same lock cancel uses. Thus a cancellation accepted first
            // prevents user shell code from starting.
            let _publication_lock = Lock::acquire_blocking("publication")?;
            if cancellation_requested(id) {
                return Ok(true);
            }
            let mut running_check = active_check;
            running_check.pid = Some(pid);
            running_check.process_start = Some(process_start_identity(pid)?);
            running_check.published_at_ms = unix_time_ms();
            publish_active_check(&running_check)?;
            if let Some(parent) = gate_for_publish.parent() {
                fs::create_dir_all(parent)?;
            }
            let gate = fs::File::create(&gate_for_publish)?;
            gate.sync_all()?;
            running_check.gate_opened = true;
            publish_active_check(&running_check)?;
            Ok(false)
        },
    )?;
    let check_elapsed_ms = check_started_at.elapsed().as_millis();
    crate::runlog::publish_attempt_log(&log_path)?;

    // The check may have modified files in the runner workspace (build
    // artifacts, formatters). Snapshot them into the workspace commit so the
    // revset-based operations below see the post-check tree.
    jj::snapshot_workspace(&workspace_path)?;

    if check_status.cancelled || cancellation_requested(id) {
        let _publication_lock = Lock::acquire_blocking("publication")?;
        cancel_serial_attempt(
            id,
            &candidate_change_id,
            &candidate_commit_id,
            &trunk_commit_id,
            strategy,
            &run_name,
            &run_config.run_token,
            &log_path,
            &rebase_duplicate_ids,
            check_elapsed_ms,
        )?;
        return Ok(RunResult::Canceled);
    }

    if !check_status.status.success() {
        let _publication_lock = Lock::acquire_blocking("publication")?;
        if cancellation_requested(id) {
            cancel_serial_attempt(
                id,
                &candidate_change_id,
                &candidate_commit_id,
                &trunk_commit_id,
                strategy,
                &run_name,
                &run_config.run_token,
                &log_path,
                &rebase_duplicate_ids,
                check_elapsed_ms,
            )?;
            return Ok(RunResult::Canceled);
        }
        // Print log output (skipping sentinel lines)
        if let Ok(log_contents) = fs::read_to_string(&log_path) {
            for line in log_contents.lines() {
                if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                    eprintln!("{}", line);
                }
            }
        }

        crate::runlog::archive_failed(id, &log_path)?;

        // Failed entry before claim delete: a crash between the two
        // duplicates the item instead of losing it.
        jj::bookmark_create_internal(&queue::failed_bookmark(id), &workspace_rev)?;
        jj::bookmark_delete(&running_bookmark)?;
        jj::describe(
            &workspace_rev,
            &failure_description(id, "check", &failure_context, &[]),
        )?;
        let mut event = Event::new(id, kind::FAILED);
        event.candidate_change_id = Some(candidate_change_id.clone());
        event.candidate_commit_id = Some(candidate_commit_id.clone());
        event.trunk_commit_id = Some(trunk_commit_id.clone());
        event.strategy = Some(strategy.as_str().to_string());
        event.failure_reason = Some("check".to_string());
        event.elapsed_ms = Some(check_elapsed_ms);
        record_history(event)?;
        remove_active_check(&run_config.run_token, id);
        remove_attempt_log(&log_path);

        let _ = runner_workspace.keep();

        preferr(&format!("merge {} failed check, marked as failed", id));
        preferr(&format!("  candidate: {}", candidate_change_id));
        preferr("");
        preferr("To resolve:");
        preferr(&format!("  # fix the issue in {}", candidate_change_id));
        preferr(&format!("  jjq push {}", candidate_change_id));
        return Ok(RunResult::Failure);
    }

    // A successful check becomes a receipt. Its checked revision is captured
    // after snapshotting, so merge preserves the existing behavior of landing
    // check-created artifacts. Rebase deliberately lands the original
    // candidate rather than artifacts written into its disposable duplicate.
    let receipt = ValidationReceipt::new(
        id,
        candidate_change_id.clone(),
        candidate_commit_id.clone(),
        trunk_bookmark.clone(),
        trunk_commit_id.clone(),
        materialized_result_commit_id,
        jj::get_commit_id(&workspace_rev)?,
        strategy,
        &run_config.check_command,
    );

    // Serialize the fresh config read and landing decision with all metadata
    // writers. Otherwise `config set` could win between validation and the
    // trunk CAS, landing work checked under a command that no longer governs
    // the queue. Drop before stale cleanup/history because those paths may
    // write metadata themselves.
    let _publication_lock = Lock::acquire_blocking("publication")?;
    if cancellation_requested(id) {
        cancel_serial_attempt(
            id,
            &candidate_change_id,
            &candidate_commit_id,
            &trunk_commit_id,
            strategy,
            &run_name,
            &run_config.run_token,
            &log_path,
            &rebase_duplicate_ids,
            check_elapsed_ms,
        )?;
        return Ok(RunResult::Canceled);
    }
    let meta_lock = meta::acquire()?;

    // Consume a receipt only after all of its safety facts still match. This
    // is immediately before any landing mutation, especially before rebase
    // rewrites the original candidate. A stale but unchanged claim returns to
    // its same FIFO position for a fresh materialization and check.
    if let Err(error) = validate_receipt(&receipt, &workspace_rev) {
        drop(meta_lock);
        discard_receipt_work(
            &receipt,
            strategy,
            &rebase_duplicate_ids,
            &run_name,
            &error.to_string(),
            false,
        )?;
        remove_active_check(&run_config.run_token, id);
        remove_attempt_log(&log_path);
        return Ok(RunResult::StaleReceipt);
    }
    receipt_validated_test_hook()?;

    // Success path — order matters for crash safety:
    // 1. Move trunk first (most critical)
    // 2. Delete the running claim
    // 3. Describe commit
    // For rebase: we tested against a duplicate, now rebase the original to
    // preserve change ID, then move trunk to the rebased original.

    let result_change_id = match strategy {
        config::Strategy::Merge => {
            // Land the exact workspace commit that the receipt checked, not a
            // workspace-qualified revset that another operation could rewrite.
            let landed_change_id = jj::resolve_revset(&receipt.checked_result_commit_id)?;
            if let Err(error) = jj::bookmark_move(
                &receipt.trunk_bookmark,
                &receipt.trunk_commit_id,
                &receipt.checked_result_commit_id,
            ) {
                drop(meta_lock);
                discard_receipt_work(
                    &receipt,
                    strategy,
                    &rebase_duplicate_ids,
                    &run_name,
                    &format!("trunk changed while landing: {}", error),
                    false,
                )?;
                remove_active_check(&run_config.run_token, id);
                remove_attempt_log(&log_path);
                return Ok(RunResult::StaleReceipt);
            }
            landing_cas_test_hook()?;
            if !queue::running_item_exists(id)? {
                preferr(&format!(
                    "note: item {} was removed during landing; it landed anyway",
                    id
                ));
            }
            jj::bookmark_delete(&running_bookmark)?;
            jj::describe(
                &receipt.checked_result_commit_id,
                &format!("Success: merge {}", id),
            )?;
            jj::workspace_forget(&run_name)?;

            prefout(&format!(
                "merged {} to {} (now at {})",
                id, receipt.trunk_bookmark, landed_change_id
            ));
            landed_change_id
        }
        config::Strategy::Rebase => {
            // The duplicate passed checks. Now rebase the ORIGINAL candidate
            // onto trunk to preserve its change ID.

            // Rebase the exact candidate and base bound into the receipt.
            // Do not use the mutable running bookmark/change ID as a landing
            // operand after receipt validation.
            jj::rebase_branch_onto(&receipt.candidate_commit_id, &receipt.trunk_commit_id)?;

            // Resolve the rewritten candidate once, then pin that exact output
            // for the remaining tree check and trunk CAS. Rebase intentionally
            // ignores check-created artifacts, so compare it with the
            // materialized pre-check tree rather than the post-check duplicate.
            let (rebased_change_id, rebased_commit_id) = jj::resolve_revset_full(&format!(
                "bookmarks(exact:{})",
                queue::running_bookmark(receipt.sequence_id)
            ))?;
            if rebased_change_id != receipt.candidate_change_id
                || !jj::trees_match(&receipt.materialized_result_commit_id, &rebased_commit_id)?
            {
                drop(meta_lock);
                discard_receipt_work(
                    &receipt,
                    strategy,
                    &rebase_duplicate_ids,
                    &run_name,
                    "rebased result no longer matches the receipt materialization",
                    true,
                )?;
                remove_active_check(&run_config.run_token, id);
                remove_attempt_log(&log_path);
                return Ok(RunResult::StaleReceipt);
            }

            // Move trunk to the exact rebased candidate, never a live change
            // ID that may be amended after the rebase command returns.
            if let Err(error) = jj::bookmark_move(
                &receipt.trunk_bookmark,
                &receipt.trunk_commit_id,
                &rebased_commit_id,
            ) {
                drop(meta_lock);
                discard_receipt_work(
                    &receipt,
                    strategy,
                    &rebase_duplicate_ids,
                    &run_name,
                    &format!("trunk changed while landing: {}", error),
                    true,
                )?;
                remove_active_check(&run_config.run_token, id);
                remove_attempt_log(&log_path);
                return Ok(RunResult::StaleReceipt);
            }
            landing_cas_test_hook()?;
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
            jj::describe(&rebased_commit_id, &desc)?;

            // Abandon all duplicates (they were only used for testing)
            for dup_id in &rebase_duplicate_ids {
                jj::abandon(dup_id)?;
            }

            jj::workspace_forget(&run_name)?;

            prefout(&format!(
                "rebased {} to {} (now at {})",
                id, receipt.trunk_bookmark, rebased_change_id
            ));
            rebased_change_id
        }
    };

    // The landing decision and CAS are complete. History recording acquires
    // the same lock internally, so release it before emitting the audit event.
    drop(meta_lock);

    // Describing the landed revision rewrites its commit ID after the trunk
    // CAS. Resolve from the stable change ID now so history names the commit
    // actually visible at trunk, not its pre-description predecessor.
    let (result_change_id, result_commit_id) =
        jj::resolve_revset_full(&format!("change_id({})", result_change_id))?;

    let mut event = Event::new(id, kind::LANDED);
    event.candidate_change_id = Some(candidate_change_id.clone());
    event.candidate_commit_id = Some(candidate_commit_id.clone());
    event.trunk_commit_id = Some(trunk_commit_id.clone());
    event.result_commit_id = Some(result_commit_id);
    event.result_change_id = Some(result_change_id);
    event.strategy = Some(strategy.as_str().to_string());
    event.elapsed_ms = Some(check_elapsed_ms);
    record_history(event)?;
    remove_active_check(&run_config.run_token, id);
    remove_attempt_log(&log_path);

    // A landed change that is still some workspace's working-copy commit
    // will be amended by that workspace's next snapshot, and trunk follows
    // the rewrite (directly under rebase; via descendant auto-rebase of the
    // merge commit under merge). push moves the pushing workspace off its
    // own @, but a commit pushed from a different workspace can still land
    // live — and only a jj command run inside that workspace could move it,
    // which would itself snapshot pending edits into the landed change. All
    // this runner can do is warn. Best-effort: the land already succeeded.
    for ws_name in jj::workspaces_at(&candidate_change_id).unwrap_or_default() {
        preferr(&format!(
            "warning: landed change {} is the working-copy commit of workspace '{}'",
            candidate_change_id, ws_name
        ));
        preferr(&format!(
            "  run `jj new` in that workspace before editing, or {} will absorb the edits",
            trunk_bookmark
        ));
    }

    Ok(RunResult::Success)
}

struct FailureContext<'a> {
    candidate_change_id: &'a str,
    candidate_commit_id: &'a str,
    trunk_commit_id: &'a str,
    workspace_path: &'a std::path::Path,
    strategy: &'a Strategy,
}

/// Finish a serial cancellation while publication.lock is held.
#[allow(clippy::too_many_arguments)]
fn cancel_serial_attempt(
    id: u32,
    candidate_change_id: &str,
    candidate_commit_id: &str,
    trunk_commit_id: &str,
    strategy: Strategy,
    run_name: &str,
    run_token: &str,
    log_path: &std::path::Path,
    rebase_duplicate_ids: &[String],
    elapsed_ms: u128,
) -> Result<()> {
    record_cancellation_terminal(
        id,
        candidate_change_id,
        candidate_commit_id,
        Some(trunk_commit_id),
        Some(strategy),
        Some(elapsed_ms),
    )?;
    if queue::running_item_exists(id)? {
        jj::bookmark_delete(&queue::running_bookmark(id))?;
    }
    for duplicate_id in rebase_duplicate_ids {
        let _ = jj::abandon(duplicate_id);
    }
    let _ = jj::workspace_forget(run_name);
    remove_active_check(run_token, id);
    remove_attempt_log(log_path);
    remove_cancellation(id);
    prefout(&format!("canceled queue item {}", id));
    Ok(())
}

fn failure_description(
    id: u32,
    reason: &str,
    context: &FailureContext<'_>,
    conflict_paths: &[String],
) -> String {
    let mut desc = format!(
        "Failed: merge {} ({})\n\njjq-candidate: {}\njjq-candidate-commit: {}\njjq-trunk: {}\njjq-workspace: {}\njjq-failure: {}\njjq-strategy: {}",
        id,
        reason,
        context.candidate_change_id,
        context.candidate_commit_id,
        context.trunk_commit_id,
        context.workspace_path.display(),
        reason,
        context.strategy.as_str()
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

fn build_item_from_state(entry: queue::StateEntry) -> Result<QueueItem> {
    Ok(QueueItem {
        id: entry.id,
        change_id: entry.change_id,
        commit_id: entry.commit_id,
        description: entry.description.lines().next().unwrap_or("").to_string(),
        allow_stack: history::allow_stack(entry.id).unwrap_or(false),
    })
}

/// Build a FailedItem by parsing trailers from the bookmark target description.
fn build_failed_item(id: u32) -> Result<FailedItem> {
    let bookmark = queue::failed_bookmark(id);
    let revset = format!("bookmarks(exact:{})", bookmark);
    let desc = jj::get_description(&revset)?;
    let mut item = build_failed_item_from_description(id, &desc)?;
    item.allow_stack = history::allow_stack(id).unwrap_or(false);
    Ok(item)
}

fn build_failed_item_from_state(entry: queue::StateEntry) -> Result<FailedItem> {
    let mut item = build_failed_item_from_description(entry.id, &entry.description)?;
    item.allow_stack = history::allow_stack(entry.id).unwrap_or(false);
    Ok(item)
}

/// Parse failed-item trailer facts from a failure description. Split out of
/// `build_failed_item_from_description` as a pure function: the full builder
/// resolves the candidate description through `jj`, which under `cargo test`
/// would snapshot the developer's real repository.
fn parse_failed_item_trailers(desc: &str) -> HashMap<String, String> {
    extract_trailers(desc)
}

fn build_failed_item_from_description(id: u32, desc: &str) -> Result<FailedItem> {
    let trailers = parse_failed_item_trailers(desc);

    let candidate_change_id = trailers.get("candidate").cloned().unwrap_or_default();
    let candidate_commit_id = trailers
        .get("candidate-commit")
        .cloned()
        .unwrap_or_default();
    let trunk_commit_id = trailers.get("trunk").cloned().unwrap_or_default();
    let workspace_path = trailers.get("workspace").cloned().unwrap_or_default();
    let failure_reason = trailers.get("failure").cloned().unwrap_or_default();
    let reject_reason = trailers.get("reject-reason").cloned().unwrap_or_default();
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
        reject_reason,
        allow_stack: false,
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
                let desc_line = description.lines().next().unwrap_or("").to_string();
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

    let Some(config_snapshot) = config::snapshot()? else {
        if json {
            let output = StatusOutput {
                runner_active: false,
                running: vec![],
                queue: vec![],
                failed: vec![],
                landed: vec![],
                active_checks: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            prefout("jjq not initialized. Run 'jjq init' first.");
        }
        return Ok(());
    };

    let runner_active = lock::is_held("run")?;

    let state = queue::state_snapshot()?;
    let running_items: Vec<QueueItem> = state
        .running()?
        .into_iter()
        .map(build_item_from_state)
        .collect::<Result<_>>()?;
    let queue_items: Vec<QueueItem> = state
        .queued()?
        .into_iter()
        .map(build_item_from_state)
        .collect::<Result<_>>()?;
    let failed_items: Vec<FailedItem> = state
        .failed()?
        .into_iter()
        .map(build_failed_item_from_state)
        .collect::<Result<_>>()?;

    let trunk_bookmark = config_snapshot.trunk_bookmark()?;
    let landed_items = get_recently_landed(&trunk_bookmark, 5).unwrap_or_default();
    let mut checks = active_checks();
    // PID publication precedes opening the start gate for crash ownership.
    // Do not present that worker as checking until the durable gate handshake
    // is complete.
    for check in &mut checks {
        if !check.gate_opened {
            check.pid = None;
            check.process_start = None;
        }
    }
    let checks = checks
        .into_iter()
        .map(ActiveCheckStatus::from)
        .collect::<Vec<_>>();

    if json {
        let output = StatusOutput {
            runner_active,
            running: running_items,
            queue: queue_items,
            failed: failed_items,
            landed: landed_items,
            active_checks: checks,
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

        if !checks.is_empty() {
            prefout("Checks:");
            for check in &checks {
                println!(
                    "  {}: {} ({} ms, base {}, speculative: {})",
                    check.id,
                    check.phase,
                    check.elapsed_ms,
                    check.predicted_base,
                    check.speculative
                );
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
    // Running is a reportable state like queued/failed: the USAGE(10) exit is
    // reserved for IDs that exist in no namespace.
    enum ItemState {
        Queued,
        Failed,
        Running,
    }
    let (state, entry) = if let Some(id_str) = id {
        let id = queue::parse_seq_id(id_str)?;
        match queue::state_snapshot()?.state_for(id)? {
            Some((queue::State::Queued, entry)) => (ItemState::Queued, entry),
            Some((queue::State::Failed, entry)) => (ItemState::Failed, entry),
            Some((queue::State::Running, entry)) => (ItemState::Running, entry),
            None => {
                return Err(ExitError::new(
                    exit_codes::USAGE,
                    format!("item {} not found in queue or failed", id),
                )
                .into());
            }
        }
    } else if let Some(change_id) = resolve {
        let (found_state, entry) = find_by_change_id(change_id)?;
        match found_state {
            queue::State::Queued => (ItemState::Queued, entry),
            queue::State::Running => (ItemState::Running, entry),
            queue::State::Failed => (ItemState::Failed, entry),
        }
    } else {
        unreachable!()
    };

    if !matches!(state, ItemState::Failed) {
        let item = build_item_from_state(entry)?;
        if json {
            // The queue and running namespaces share this item shape; an
            // explicit state field lets machine consumers tell them apart.
            #[derive(Serialize)]
            struct SingleItemStatus<'a> {
                #[serde(flatten)]
                item: &'a QueueItem,
                state: &'a str,
            }
            let state_name = match state {
                ItemState::Queued => "queued",
                ItemState::Running => "running",
                ItemState::Failed => unreachable!("failed handled below"),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&SingleItemStatus {
                    item: &item,
                    state: state_name,
                })?
            );
        } else {
            if matches!(state, ItemState::Running) {
                println!("Queue item {} (running)", item.id);
            } else {
                println!("Queue item {}", item.id);
            }
            println!("  Change ID:   {}", item.change_id);
            println!("  Commit ID:   {}", item.commit_id);
            println!("  Description: {}", item.description);
        }
    } else {
        let item = build_failed_item_from_state(entry)?;
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
            if !item.reject_reason.is_empty() {
                println!("  Rejected:    {}", item.reject_reason);
            }
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

/// Find a queue, running, or failed item by candidate change ID.
/// Returns the concrete queue state so callers report claims accurately.
fn find_by_change_id(change_id: &str) -> Result<(queue::State, queue::StateEntry)> {
    let state = queue::state_snapshot()?;
    for entry in state.queued()? {
        if entry.change_id == change_id {
            return Ok((queue::State::Queued, entry));
        }
    }

    // Match state_for's precedence (queued → failed → running): a crash
    // during failure publication can leave both failed and running
    // bookmarks, and reporting failed keeps --resolve consistent with
    // status <id>.
    for entry in state.failed()? {
        let trailers = extract_trailers(&entry.description);
        if trailers.get("candidate").map(|s| s.as_str()) == Some(change_id) {
            return Ok((queue::State::Failed, entry));
        }
    }

    // A running claim keeps the same candidate change ID as its queued
    // predecessor; report it instead of pretending the item does not exist.
    for entry in state.running()? {
        if entry.change_id == change_id {
            return Ok((queue::State::Running, entry));
        }
    }

    bail!("no item found with candidate change ID '{}'", change_id)
}

/// Find a queued submission for a stable change ID. This is used to finish a
/// requeue which was interrupted after creating its new bookmark but before
/// the old failed bookmark could be removed.
fn queued_submission_for_change(change_id: &str) -> Result<Option<(u32, String, String)>> {
    for entry in queue::state_snapshot()?.queued()? {
        if entry.change_id == change_id {
            return Ok(Some((entry.id, entry.change_id, entry.commit_id)));
        }
    }
    Ok(None)
}

/// Finish the deletion side of a requeue after its new queue bookmark exists.
/// The two linked audit events are deliberately one metadata write: a retry
/// that observes both old and new bookmarks can complete exactly this step.
fn finish_requeue(
    old_id: u32,
    new_id: u32,
    candidate_change_id: &str,
    candidate_commit_id: &str,
    trunk_commit_id: &str,
) -> Result<()> {
    // Record the linked audit events before the failed bookmark disappears
    // (create-before-delete): a crash then leaves both bookmarks, and a
    // retried requeue reaches the queued-submission branch and completes the
    // removal idempotently. The old order - remove, then record - lost the
    // requeued/queued events on a crash in between, and a retry stopped at
    // "item is already queued" without ever completing recovery.
    let already_recorded = history::read(old_id)?.iter().any(|event| {
        event.kind == kind::REQUEUED && event.related_sequence_id == Some(new_id)
    });
    if !already_recorded {
        let exempt = history::allow_stack(old_id)?;
        let mut requeued = Event::new(old_id, kind::REQUEUED);
        requeued.candidate_change_id = Some(candidate_change_id.to_string());
        requeued.candidate_commit_id = Some(candidate_commit_id.to_string());
        requeued.trunk_commit_id = Some(trunk_commit_id.to_string());
        requeued.related_sequence_id = Some(new_id);
        requeued.allow_stack = exempt.then_some(true);

        let mut queued = Event::new(new_id, kind::QUEUED);
        queued.candidate_change_id = Some(candidate_change_id.to_string());
        queued.candidate_commit_id = Some(candidate_commit_id.to_string());
        queued.trunk_commit_id = Some(trunk_commit_id.to_string());
        queued.related_sequence_id = Some(old_id);
        queued.allow_stack = exempt.then_some(true);
        history::record_many(&[requeued, queued])?;
    }
    remove_failed_entry(old_id)?;

    // Forget the run workspace by its registered name. Runner workspaces are
    // named jjq-run-NNNNNN-<run_token>; the legacy bare-ID guess never
    // matched and left a stale registration pointing at the deleted
    // directory. cleanup_run_workspace resolves the name from metadata and
    // keeps a surviving check child's directory.
    cleanup_run_workspace(old_id);
    Ok(())
}

/// Re-push a failed item back onto the queue.
pub fn requeue(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;

    require_initialized()?;
    let _mutation_lock = Lock::acquire_blocking("mutation")?;

    let state = queue::state_snapshot()?;
    let failed_entry = state.entry(queue::State::Failed, id)?;
    if failed_entry.is_none() {
        if state.entry(queue::State::Queued, id)?.is_some() {
            return Err(ExitError::new(
                exit_codes::USAGE,
                format!("item {} is already queued", id),
            )
            .into());
        }
        if state.entry(queue::State::Running, id)?.is_some() {
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
            if recover_claim(id)? {
                prefout(&format!(
                    "completed cancellation of interrupted item {}",
                    id
                ));
            } else {
                prefout(&format!(
                    "recovered interrupted item {}; it is queued again",
                    id
                ));
            }
            return Ok(());
        }
        return Err(
            ExitError::new(exit_codes::USAGE, format!("failed item {} not found", id)).into(),
        );
    }

    // Build the failed item to get candidate info
    let item = build_failed_item_from_state(failed_entry.unwrap())?;
    let candidate_change_id = &item.candidate_change_id;

    if candidate_change_id.is_empty() {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!("failed item {} has no candidate change ID", id),
        )
        .into());
    }

    // Get trunk bookmark for conflict check
    let trunk_bookmark = config::get_trunk_bookmark()?;
    if !jj::bookmark_exists(&trunk_bookmark)? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!("trunk bookmark '{}' not found", trunk_bookmark),
        )
        .into());
    }
    let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))?;

    // A previous invocation may have crashed after create-before-delete.
    // Reuse that already allocated queue ID rather than minting a duplicate,
    // then record both linked events only after the old failed state is gone.
    let publication_lock = Lock::acquire_blocking("publication")?;
    if let Some((new_id, queued_change_id, queued_commit_id)) =
        queued_submission_for_change(candidate_change_id)?
    {
        finish_requeue(
            id,
            new_id,
            candidate_change_id,
            &queued_commit_id,
            &trunk_commit_id,
        )?;
        drop(publication_lock);
        let repo_path = jj::primary_workspace_root()?;
        prefout(&format!(
            "requeued failed item {} as {} (trunk: {} in {})",
            id,
            new_id,
            trunk_bookmark,
            repo_path.display()
        ));
        move_working_copy_off_queued(&queued_change_id)?;
        return Ok(());
    }
    drop(publication_lock);

    // Resolve the candidate — it may have been rebased since the failure.
    let revset = candidate_change_id.as_str();
    let (_change_id, commit_id) = jj::resolve_revset_full(revset)
        .map_err(|e| ExitError::new(exit_codes::USAGE, format!("candidate not found: {}", e)))?;

    // Pre-flight conflict check against current trunk. Test the pinned
    // commit so the merge exercised here is exactly the one queued below.
    let conflict_check_id = jj::new_rev(&[&trunk_bookmark, &commit_id])?;
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
        return Err(ExitError::new(exit_codes::CONFLICT, "revision conflicts with trunk").into());
    }

    // The landing policy re-applies on requeue with the same exemption the
    // failed item carried.
    check_landing_policy(&commit_id, &trunk_commit_id, history::allow_stack(id)?)?;

    // Preserve create-before-delete crash safety. The cleanup and both audit
    // records happen below; a crash before then is recovered by the existing
    // queued-submission branch above.
    let publication_lock = Lock::acquire_blocking("publication")?;
    let new_id = queue::next_id()?;
    let new_bookmark = queue::queue_bookmark(new_id);
    jj::bookmark_create_internal(&new_bookmark, &commit_id)?;
    finish_requeue(
        id,
        new_id,
        candidate_change_id,
        &commit_id,
        &trunk_commit_id,
    )?;
    drop(publication_lock);

    let repo_path = jj::primary_workspace_root()?;
    prefout(&format!(
        "requeued failed item {} as {} (trunk: {} in {})",
        id,
        new_id,
        trunk_bookmark,
        repo_path.display()
    ));

    move_working_copy_off_queued(&_change_id)?;

    Ok(())
}

/// Cooperatively cancel a queued or running submission.
pub fn cancel(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;
    require_initialized()?;
    let _mutation_lock = Lock::acquire_blocking("mutation")?;
    let _publication_lock = Lock::acquire_blocking("publication")?;

    let prior = history::read(id)?;
    if prior.iter().any(|event| event.kind == kind::CANCELED) {
        if queue::queue_item_exists(id)? {
            jj::bookmark_delete(&queue::queue_bookmark(id))?;
        }
        if queue::running_item_exists(id)? {
            jj::bookmark_delete(&queue::running_bookmark(id))?;
        }
        cleanup_run_workspace(id);
        remove_cancellation(id);
        prefout(&format!("queue item {} is already canceled", id));
        return Ok(());
    }

    if queue::queue_item_exists(id)? {
        publish_cancellation(id)?;
        let revset = format!("bookmarks(exact:{})", queue::queue_bookmark(id));
        let mut requested = history_for_revision(id, kind::CANCEL_REQUESTED, &revset)?;
        requested.trunk_commit_id = jj::get_commit_id(&format!(
            "bookmarks(exact:{})",
            config::get_trunk_bookmark()?
        ))
        .ok();
        let mut canceled = requested.clone();
        canceled.kind = kind::CANCELED.to_string();
        // The publication transaction makes queued removal immediate. Record
        // both facts before deleting the authoritative bookmark so a crash
        // cannot erase why the item disappeared.
        history::record_many(&[requested, canceled])?;
        queued_cancel_recorded_test_hook()?;
        jj::bookmark_delete(&queue::queue_bookmark(id))?;
        if queue::running_item_exists(id)? {
            jj::bookmark_delete(&queue::running_bookmark(id))?;
        }
        remove_cancellation(id);
        prefout(&format!("canceled queued item {}", id));
        return Ok(());
    }

    if queue::running_item_exists(id)? {
        let newly_published = publish_cancellation(id)?;
        if newly_published {
            running_cancel_marked_test_hook()?;
            let requested = history_for_revision(
                id,
                kind::CANCEL_REQUESTED,
                &format!("bookmarks(exact:{})", queue::running_bookmark(id)),
            )?;
            record_history(requested)?;
        }
        prefout(&format!("cancellation requested for running item {}", id));
        return Ok(());
    }

    if queue::failed_item_exists(id)? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            format!(
                "item {} has already failed; use jjq delete or jjq requeue",
                id
            ),
        )
        .into());
    }

    Err(ExitError::new(exit_codes::USAGE, format!("item {} not found", id)).into())
}

/// Delete an item from queue or failed list.
pub fn delete(id_str: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;

    require_initialized()?;
    let _mutation_lock = Lock::acquire_blocking("mutation")?;

    // Check queue first. Publication serializes the deletion against the
    // runner's claim window (the queue -> running rename takes the same
    // lock), so a delete cannot observe a claim appearing mid-mutation and
    // strip a live runner's claim.
    let publication_lock = Lock::acquire_blocking("publication")?;
    if queue::queue_item_exists(id)? {
        // A legacy dual state (queue and running bookmarks both name this
        // ID) needs the run lock before either deletion: a runner that
        // starts between deleting the queue bookmark and handling the claim
        // recovers the claim back to the queue and re-claims the supposedly
        // deleted item. Runners take run before publication, so run cannot
        // be acquired while holding publication — release it, take run
        // (refusing when a live runner holds it), then re-acquire
        // publication. With mutation and run both held nothing else can
        // touch these bookmarks, so no re-validation is needed.
        if queue::running_item_exists(id)? {
            drop(publication_lock);
            let _run_lock = Lock::acquire("run")?.ok_or_else(|| {
                ExitError::new(
                    exit_codes::USAGE,
                    format!(
                        "item {} is being processed by a running queue runner (jjq delete can only remove it after the run finishes)",
                        id
                    ),
                )
            })?;
            let publication_lock = Lock::acquire_blocking("publication")?;
            let event = history_for_revision(
                id,
                kind::DELETED,
                &format!("bookmarks(exact:{})", queue::queue_bookmark(id)),
            )?;
            jj::bookmark_delete(&queue::queue_bookmark(id))?;
            jj::bookmark_delete(&queue::running_bookmark(id))?;
            record_history(event)?;
            drop(publication_lock);
            prefout(&format!("deleted queued item {}", id));
            return Ok(());
        }
        let event = history_for_revision(
            id,
            kind::DELETED,
            &format!("bookmarks(exact:{})", queue::queue_bookmark(id)),
        )?;
        jj::bookmark_delete(&queue::queue_bookmark(id))?;
        record_history(event)?;
        prefout(&format!("deleted queued item {}", id));
        return Ok(());
    }
    drop(publication_lock);

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

        let event = history_for_revision(
            id,
            kind::DELETED,
            &format!("bookmarks(exact:{})", queue::running_bookmark(id)),
        )?;
        jj::bookmark_delete(&queue::running_bookmark(id))?;
        record_history(event)?;
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
        let mut event = Event::new(id, kind::DELETED);
        let item = build_failed_item(id)?;
        if !item.candidate_change_id.is_empty() {
            event.candidate_change_id = Some(item.candidate_change_id);
        }
        if !item.candidate_commit_id.is_empty() {
            event.candidate_commit_id = Some(item.candidate_commit_id);
        }
        if !item.trunk_commit_id.is_empty() {
            event.trunk_commit_id = Some(item.trunk_commit_id);
        }
        remove_failed_entry(id)?;
        record_history(event)?;
        prefout(&format!("deleted failed item {}", id));

        // Forget the workspace by the name recorded at claim time (tokened)
        // and remove its directory, exactly like stale-claim cleanup.
        if let Some(path) = lookup_workspace_path(id) {
            if std::path::Path::new(&path).is_dir() {
                prefout(&format!("removed workspace {}", path));
            }
        }
        cleanup_run_workspace(id);

        return Ok(());
    }

    Err(ExitError::new(
        exit_codes::USAGE,
        format!("item {} not found in queue or failed", id),
    )
    .into())
}

/// Insert or replace one `jjq-<key>: <value>` trailer in a description,
/// keeping the trailer block as the final lines.
fn upsert_trailer(description: &str, key: &str, value: &str) -> String {
    let trailer_line = format!("jjq-{}: {}", key, value);
    let mut lines: Vec<String> = description.lines().map(str::to_string).collect();
    if let Some(existing) = lines
        .iter_mut()
        .find(|line| line.starts_with(&format!("jjq-{}: ", key)))
    {
        *existing = trailer_line;
        return lines.join("\n");
    }
    // Find the end of the trailer block: jjq-* lines at the tail.
    let mut insert_at = lines.len();
    while insert_at > 0
        && lines[insert_at - 1]
            .split_once(": ")
            .is_some_and(|(prefix, _)| prefix.starts_with("jjq-"))
    {
        insert_at -= 1;
    }
    // A non-trailer last line needs a blank separator before the trailer
    // block only if the description doesn't already end with one.
    if insert_at > 0 && !lines[insert_at - 1].is_empty() {
        lines.insert(insert_at, String::new());
        insert_at += 1;
    }
    lines.insert(insert_at, trailer_line);
    lines.join("\n")
}

/// Build the failure-marker description for a policy rejection. Mirrors
/// `failure_description` with `jjq-failure: rejected`, an empty workspace
/// (a rejection never materializes one), and the reason as its own trailer.
fn reject_description(
    id: u32,
    entry: &queue::StateEntry,
    trunk_commit_id: &str,
    strategy: &Strategy,
    reason: &str,
) -> String {
    let context = FailureContext {
        candidate_change_id: &entry.change_id,
        candidate_commit_id: &entry.commit_id,
        trunk_commit_id,
        workspace_path: std::path::Path::new(""),
        strategy,
    };
    let desc = failure_description(id, "rejected", &context, &[]);
    upsert_trailer(&desc, "reject-reason", reason)
}

/// Reject an item: mark it failed without running, with a reason. Accepts a
/// queued item (moves it to failed) or an already-failed item (attaches or
/// replaces the reason). The reason is the state bit the fix loop and
/// `jjq wait` read: a failed item with a reason is terminal.
pub fn reject(id_str: &str, reason: &str) -> Result<()> {
    let id = queue::parse_seq_id(id_str)?;
    // Single-line trailer value: collapse any whitespace runs.
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.is_empty() {
        return Err(ExitError::new(exit_codes::USAGE, "reason must not be empty").into());
    }

    require_initialized()?;
    // The same pair of locks delete() takes, in the same order. Publication
    // serializes the queue->failed move against the runner's claim rename:
    // with only the mutation lock, reject could create the failed bookmark
    // and then fail to delete a queue bookmark the runner just renamed,
    // leaving dual state. Both are held across the snapshot and both
    // bookmark mutations.
    let _mutation_lock = Lock::acquire_blocking("mutation")?;
    let _publication_lock = Lock::acquire_blocking("publication")?;

    let state = queue::state_snapshot()?;
    match state.state_for(id)? {
        Some((queue::State::Queued, entry)) => {
            let trunk_bookmark = config::get_trunk_bookmark()?;
            let trunk_commit_id = jj::get_commit_id(&format!("bookmarks(exact:{})", trunk_bookmark))
                .unwrap_or_default();
            let strategy = config::snapshot()?
                .ok_or_else(|| anyhow!("jjq metadata missing; run 'jjq doctor'"))?
                .strategy()?;
            // Marker commit: child of the candidate, described with the
            // failure trailer block plus the reject reason.
            let marker = jj::new_rev(&[&entry.commit_id])?;
            let desc = reject_description(id, &entry, &trunk_commit_id, &strategy, &reason);
            jj::describe(&marker, &desc)?;
            // Create-before-delete, the same crash ordering as the run
            // failure path: a crash here leaves both bookmarks and a retry
            // finds the failed entry first.
            jj::bookmark_create_internal(&queue::failed_bookmark(id), &marker)?;
            jj::bookmark_delete(&queue::queue_bookmark(id))?;
        }
        Some((queue::State::Failed, entry)) => {
            // Rewrite the failed marker's description, replacing any
            // existing reject-reason trailer. The failed bookmark is the
            // only pointer to this commit, so redescribing is safe.
            let new_desc = upsert_trailer(&entry.description, "reject-reason", &reason);
            jj::describe(&entry.commit_id, &new_desc)?;
        }
        Some((queue::State::Running, _)) => {
            return Err(ExitError::new(
                exit_codes::USAGE,
                format!("item {} is running; cancel it first (jjq cancel {})", id, id),
            )
            .into());
        }
        None => {
            return Err(ExitError::new(
                exit_codes::USAGE,
                format!("no queued or failed item {}", id),
            )
            .into());
        }
    }

    let mut event = Event::new(id, kind::REJECTED);
    event.failure_reason = Some(reason);
    record_history(event)?;
    Ok(())
}

/// The wait verdict for one candidate, checked in this order each poll:
/// landed (handled inline in the poll loop, since it outranks everything),
/// rejected, pending, unknown.
enum Verdict {
    /// A failed entry carries a recorded reject reason.
    Rejected { id: u32, reason: String },
    /// Queued, claimed, or failed without a reason: the fix loop may
    /// still be working it.
    Pending,
    /// The candidate matches no queue state (and did not land).
    Unknown,
}

/// Resolve the wait target tolerantly: run `jj log -r <input>` and accept
/// exactly one distinct change ID. Divergent copies of one change dedupe to
/// a single ID, which is fine — the landed check and classify both key on
/// change ID. Any jj error, zero lines, or multiple distinct IDs is not a
/// target; `wait` treats that as the Unknown verdict instead of crashing
/// through the generic error path.
fn wait_resolve_target(revset: &str) -> Option<String> {
    let output = jj::run_ok_ignore_working_copy(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        "change_id.short() ++ \"\\n\"",
    ])
    .ok()?;
    let mut ids: Vec<&str> = output.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    ids.sort_unstable();
    ids.dedup();
    match ids.len() {
        1 => Some(ids[0].to_string()),
        _ => None,
    }
}

/// True when the candidate's change ID is an ancestor of (or at) trunk.
/// Uses the `change_id()` revset function, not a bare symbol: a divergent
/// change-ID symbol errors on resolution, and a jj error here means "not
/// proven landed", which the caller treats as fall-through.
fn wait_is_landed(change_id: &str, trunk: &str) -> Result<bool> {
    let revset = format!("::bookmarks(exact:{}) & change_id({})", trunk, change_id);
    let output = jj::run_ok_ignore_working_copy(&[
        "log",
        "-r",
        &revset,
        "--no-graph",
        "-T",
        "\"x\"",
    ]);
    match output {
        Ok(text) => Ok(!text.trim().is_empty()),
        Err(_) => Ok(false),
    }
}

/// Classify one queue-state snapshot against the resolved candidate. Exact
/// short-ID matching only: resolution already normalized the input.
fn wait_classify(state: &queue::StateSnapshot, change_id: &str) -> Result<Verdict> {
    for entry in state.queued()? {
        if entry.change_id == change_id {
            return Ok(Verdict::Pending);
        }
    }
    for entry in state.running()? {
        if entry.change_id == change_id {
            return Ok(Verdict::Pending);
        }
    }
    for entry in state.failed()? {
        let item = build_failed_item_from_state(entry.clone())?;
        if item.candidate_change_id == change_id {
            return Ok(if item.reject_reason.is_empty() {
                Verdict::Pending
            } else {
                Verdict::Rejected {
                    id: entry.id,
                    reason: item.reject_reason,
                }
            });
        }
    }
    Ok(Verdict::Unknown)
}

/// Wait for a candidate to reach a terminal verdict, then print it and exit:
/// 0 landed, 1 rejected (reason and item ID on stdout), 2 unknown or timed
/// out. This is the submitter's half of the feedback loop: a development
/// agent runs `jjq push @ && jjq wait <change-id>` as a background task.
pub fn wait(revset: &str, interval_secs: u64, timeout_secs: Option<u64>) -> Result<()> {
    require_initialized()?;
    let trunk = config::get_trunk_bookmark()?;
    let deadline = timeout_secs
        .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
    let mut unknown_streak = 0u32;
    loop {
        // 0. Resolve the target inside the loop: the fixer may rewrite the
        //    candidate mid-wait, and an unresolvable input is the Unknown
        //    verdict, not a crash.
        let Some(change_id) = wait_resolve_target(revset) else {
            unknown_streak += 1;
            if unknown_streak >= 2 {
                return Err(ExitError::new(
                    2,
                    format!("revset '{}' does not resolve to one change", revset),
                )
                .into());
            }
            if wait_deadline_passed(deadline) {
                return Err(ExitError::new(2, "timed out waiting for verdict").into());
            }
            std::thread::sleep(std::time::Duration::from_secs(interval_secs));
            continue;
        };

        // 1. Landed outranks any stale queue entry or rejection.
        if wait_is_landed(&change_id, &trunk)? {
            prefout(&format!("landed: {}", change_id));
            return Ok(());
        }

        // 2/3. Classify one snapshot: rejected, pending, or unknown.
        let state = queue::state_snapshot()?;
        match wait_classify(&state, &change_id)? {
            Verdict::Rejected { id, reason } => {
                prefout(&format!("rejected: item {}: {}", id, reason));
                return Err(ExitError::code_only(1).into());
            }
            Verdict::Pending => unknown_streak = 0,
            Verdict::Unknown => {
                unknown_streak += 1;
                if unknown_streak >= 2 {
                    return Err(ExitError::new(
                        2,
                        format!("no queue entry for {}", change_id),
                    )
                    .into());
                }
            }
        }
        if wait_deadline_passed(deadline) {
            return Err(ExitError::new(2, "timed out waiting for verdict").into());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval_secs));
    }
}

fn wait_deadline_passed(deadline: Option<std::time::Instant>) -> bool {
    deadline.is_some_and(|d| std::time::Instant::now() >= d)
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
            println!(
                "require_single_revision = {}",
                snapshot.require_single_revision()?
            );
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
                    "require_single_revision" => "true".to_string(),
                    _ => unreachable!(),
                };
                println!("{}", value);
                return Ok(());
            };

            let value = match k {
                "trunk_bookmark" => snapshot.trunk_bookmark()?,
                "check_command" => snapshot.check_command()?.unwrap_or_default(),
                "strategy" => snapshot.strategy()?.as_str().to_string(),
                "require_single_revision" => snapshot.require_single_revision()?.to_string(),
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
                    &format!(
                        "sequence counter (last_id) unparseable: {:?}",
                        contents.trim()
                    ),
                );
                fails += 1;
            }
            Ok(None) => {
                print_check(
                    "FAIL",
                    "sequence counter (last_id) missing from metadata branch",
                );
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
            (is_jjq_workspace(name) && name != meta::META_WORKSPACE_NAME).then_some(())
        })
        .count();
    if orphaned == 0 {
        print_check("ok", "no orphaned workspaces");
    } else {
        print_check("WARN", &format!("{} orphaned workspace(s) found", orphaned));
        print_hint("to fix: jjq clean");
        warns += 1;
    }

    let legacy_paths = legacy_filesystem_state_paths()?;
    if !legacy_paths.is_empty() {
        let current_dir = paths::jjq_dir()?;
        print_check(
            "info",
            &format!(
                "legacy jjq filesystem state found; current state lives in {}",
                current_dir.display()
            ),
        );
        for path in legacy_paths {
            print_hint(&format!("legacy path: {}", path.display()));
        }
        print_hint("to remove legacy state: jjq clean");
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

fn legacy_locks_dir() -> Result<std::path::PathBuf> {
    Ok(jj::shared_jj_dir()?.join("jjq-locks"))
}

fn legacy_run_log() -> Result<std::path::PathBuf> {
    Ok(jj::shared_jj_dir()?.join("jjq-run.log"))
}

fn legacy_filesystem_state_paths() -> Result<Vec<std::path::PathBuf>> {
    let paths = [legacy_locks_dir()?, legacy_run_log()?];
    Ok(paths
        .into_iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>())
}

fn remove_legacy_filesystem_state() -> Result<()> {
    let legacy_locks = legacy_locks_dir()?;
    if legacy_locks.exists() {
        fs::remove_dir_all(&legacy_locks)?;
    }

    let legacy_log = legacy_run_log()?;
    if legacy_log.exists() {
        fs::remove_file(&legacy_log)?;
    }

    Ok(())
}

/// Remove all jjq workspaces and their directories.
pub fn clean() -> Result<()> {
    // Hold the run lock for the whole cleanup: an active runner's workspace
    // must not be destroyed out from under its check process. Lock order is
    // run, then check, then meta — no path takes run while holding check or
    // meta, so the order stays acyclic.
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
    refuse_if_orphan_checks_live("cleaning")?;
    prune_dead_attempt_state();

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

    // The persistent metadata workspace is disposable cache state, but it
    // must not be removed while a metadata transaction is using it.
    let _meta_lock = meta::acquire()?;

    let ws_output = jj::workspace_list()?;

    // `jjq-meta` was a valid user-chosen workspace name before jjq reserved it
    // for the persistent metadata cache. Verify ownership before cleaning any
    // workspace so a collision fails without partially applying cleanup.
    if ws_output.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|name| name.trim_end_matches(':') == meta::META_WORKSPACE_NAME)
    }) {
        meta::metadata_workspace_registration_owned()?;
    }

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
        let (label, workspace_path) = if ws_name == meta::META_WORKSPACE_NAME {
            (
                "metadata cache".to_string(),
                Some(
                    meta::metadata_workspace_path()?
                        .to_string_lossy()
                        .into_owned(),
                ),
            )
        } else if let Some(ws_id_str) = ws_name.strip_prefix("jjq-run-") {
            // Tokenized speculative workspaces are jjq-run-NNNNNN-TOKEN;
            // retain support for the legacy bare NNNNNN form.
            let plain_id: u32 = ws_id_str
                .split('-')
                .next()
                .and_then(|id| id.parse().ok())
                .unwrap_or(0);
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

    remove_legacy_filesystem_state()?;

    // Standalone residue pass: abandon the recorded candidates of landed
    // work that are still visible as leaves. Failures are warnings.
    cleanup_residue()?;

    if removed == 0 {
        prefout("no workspaces to clean");
    } else {
        let detail_str = details.join("\n");
        prefout(&format!("removed {} workspace(s)\n{}", removed, detail_str));
    }

    Ok(())
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod process_liveness_tests {
    use super::*;

    #[test]
    fn zombie_supervisor_is_not_live() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        // Confirm exit without reaping it. WNOWAIT keeps the child available
        // to native process inspection as a deliberate zombie.
        let mut exit_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut exit_info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let info = loop {
            if let Ok(info) = native_process_info(pid)
                && info.zombie
            {
                break info;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {} did not become a zombie",
                pid
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let check = ActiveCheck {
            id: 1,
            run_token: "zombie-test".to_string(),
            predicted_base: "base".to_string(),
            speculative: false,
            workspace_path: "/tmp/zombie-test".to_string(),
            log_path: "/tmp/zombie-test.log".to_string(),
            phase: "checking".to_string(),
            process_start: Some(info.start),
            pid: Some(pid),
            published_at_ms: unix_time_ms(),
            started_at_ms: unix_time_ms(),
            elapsed_ms: 0,
            gate_opened: true,
        };
        assert!(!active_check_is_live(&check));
        child.wait().unwrap();
    }
}

#[cfg(test)]
mod failed_item_tests {
    use super::{build_failed_item_from_description, parse_failed_item_trailers};

    #[test]
    fn failed_item_parses_reject_reason_trailer() {
        let desc = "Failed: merge 3 (rejected)\n\njjq-candidate: abc123\njjq-candidate-commit: def456\njjq-trunk: 789\njjq-workspace: \njjq-failure: rejected\njjq-strategy: rebase\njjq-reject-reason: 2 revisions atop trunk; squash and re-push";
        let item = build_failed_item_from_description(3, desc).unwrap();
        assert_eq!(item.reject_reason, "2 revisions atop trunk; squash and re-push");
        assert_eq!(item.failure_reason, "rejected");
    }

    #[test]
    fn failed_item_without_reject_reason_is_empty() {
        let desc = "Failed: merge 3 (check)\n\njjq-candidate: abc123\njjq-candidate-commit: def456\njjq-trunk: 789\njjq-workspace: /tmp/ws\njjq-failure: check\njjq-strategy: rebase";
        let item = build_failed_item_from_description(3, desc).unwrap();
        assert!(item.reject_reason.is_empty());
    }

    #[test]
    fn trailer_parsing_exposes_reject_reason_key() {
        let trailers = parse_failed_item_trailers(
            "Failed: merge 4 (rejected)\n\njjq-failure: rejected\njjq-reject-reason: nothing to land atop trunk",
        );
        assert_eq!(trailers.get("reject-reason").map(String::as_str), Some("nothing to land atop trunk"));
        assert_eq!(trailers.get("failure").map(String::as_str), Some("rejected"));
    }
}
