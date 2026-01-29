# Rust Port Catchup Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Bring the Rust port (`.workspaces/rust-impl`) to parity with the bash reference implementation and specification, covering the changes from `mtloxrko` through `wxvowwlq`.

**Architecture:** The Rust port already has good structure (main.rs, commands.rs, config.rs, queue.rs, jj.rs, lock.rs). Changes are surgical — remove the `retry` command, add idempotent push cleanup, add `jjq-candidate:` trailers to failure descriptions, add resolution guidance output, make `run --all` continue past failures, add workspace cleanup to `delete`, and add the `clean` command. Exit codes need to be explicit and match the spec.

**Tech Stack:** Rust 2024 edition, clap 4, anyhow, tempfile, regex, insta (snapshot tests)

---

## Delta Summary

The bash script evolved significantly since the Rust port was written. Here's what changed and what the Rust port needs:

| Feature | Bash Status | Rust Status | Action Needed |
|---------|------------|-------------|---------------|
| `retry` command | **Removed** | Exists | Remove entirely |
| `push` idempotent cleanup | Implemented | Missing | Add change_id/commit_id scanning |
| `push` preflight uses `jj new --no-edit` | Implemented | Uses workspace | Switch to headless merge commit |
| `jjq-candidate:` trailer on failures | Implemented | Missing | Add to conflict/check failure descriptions |
| Resolution guidance output | Implemented | Missing | Add "To resolve:" messages |
| `run --all` continues past failures | Implemented | Stops on first failure | Track failed_count, continue |
| `run --all` bails on lock held | Implemented | N/A (stops on failure) | Add EXIT_LOCK_HELD special case |
| Exit code constants | Defined (0,1,2,3,4,10) | Only uses 0/1 | Add all exit codes, use them |
| `delete` workspace cleanup | Implemented | Missing | Add workspace forget + dir removal |
| `clean` command | Implemented | Missing | Add command |
| `log_op` metadata logging | Implemented | Missing | Add (low priority, skip for now) |
| `lookup_workspace_path` | Implemented | Missing | Add for delete/clean |

---

## Task 1: Add Exit Code Constants

**Files:**
- Create: `src/exit_codes.rs`
- Modify: `src/main.rs:56-61`

**Step 1: Create exit_codes.rs**

```rust
// ABOUTME: Exit code constants matching the jjq specification.
// ABOUTME: Used throughout the codebase for consistent process exit codes.

pub const SUCCESS: i32 = 0;
pub const CONFLICT: i32 = 1;
pub const CHECK_FAILED: i32 = 2;
pub const LOCK_HELD: i32 = 3;
pub const TRUNK_MOVED: i32 = 4;
pub const USAGE: i32 = 10;

/// Internal sentinel — not returned to the user.
pub const QUEUE_EMPTY: i32 = 99;
```

**Step 2: Register the module in main.rs**

Add `mod exit_codes;` to the module list in `main.rs`.

**Step 3: Build to verify it compiles**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo build`
Expected: Build succeeds

**Step 4: Commit**

```
jj desc -m "Add exit code constants module"
```

---

## Task 2: Refactor commands.rs to Use Exit Codes and Return Them

The bash script exits with specific codes (1 for conflict, 2 for check failure, 3 for lock held, etc.). The Rust port currently uses `anyhow::bail!` for everything, which always exits with code 1. We need a mechanism to return specific exit codes.

**Files:**
- Modify: `src/main.rs`
- Modify: `src/commands.rs`

**Step 1: Add a JjqError type to commands.rs for exit-code-bearing errors**

At the top of `commands.rs`, add:

```rust
use crate::exit_codes;

/// Error type that carries a specific exit code.
pub struct ExitError {
    pub code: i32,
    pub message: String,
}

impl ExitError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        ExitError { code, message: message.into() }
    }
}

impl From<ExitError> for anyhow::Error {
    fn from(e: ExitError) -> Self {
        anyhow::anyhow!(e.message)
    }
}
```

**Step 2: Update main.rs to extract exit codes**

Change the `main()` and `run()` functions in `main.rs` to handle `ExitError`:

```rust
fn main() {
    if let Err(e) = run() {
        if let Some(exit_err) = e.downcast_ref::<ExitError>() {
            eprintln!("jjq: {}", exit_err.message);
            std::process::exit(exit_err.code);
        }
        eprintln!("jjq: {}", e);
        std::process::exit(1);
    }
}
```

Move `ExitError` definition into `exit_codes.rs` so main.rs can reference it:

```rust
// In exit_codes.rs, add:
use std::fmt;

#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub message: String,
}

impl ExitError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        ExitError { code, message: message.into() }
    }
}

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExitError {}
```

**Step 3: Update run_one() to return specific exit codes**

In the conflict branch of `run_one()`, change `bail!` calls:
- Conflicts: return `ExitError::new(exit_codes::CONFLICT, ...)`
- Check failure: return `ExitError::new(exit_codes::CHECK_FAILED, ...)`
- Lock held: return `ExitError::new(exit_codes::LOCK_HELD, ...)`
- Trunk moved: return `ExitError::new(exit_codes::TRUNK_MOVED, ...)`

The `RunResult` enum needs updating — instead of `Failure(String)`, use `Failure(i32, String)` to carry the exit code:

```rust
enum RunResult {
    Success,
    Empty,
    Failure(i32, String),
}
```

Update all `RunResult::Failure` construction sites and the match in `run()`.

**Step 4: Update push() for exit codes**

- Conflict with trunk: `ExitError::new(exit_codes::CONFLICT, "revision conflicts with trunk")`
- Usage errors (revset not found, etc.): `ExitError::new(exit_codes::USAGE, ...)`

**Step 5: Build and run existing tests**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: Some snapshot tests may need updating due to changed exit codes. Update snapshots.

**Step 6: Commit**

```
jj desc -m "Use specific exit codes throughout commands"
```

---

## Task 3: Remove the `retry` Command

The `retry` command has been removed from the bash script and spec. Users now fix their revision and re-push (idempotent push handles the rest).

**Files:**
- Modify: `src/main.rs:35-41` (remove Retry variant from Commands enum)
- Modify: `src/main.rs:73` (remove Retry match arm)
- Modify: `src/commands.rs:294-333` (delete `retry` function)
- Modify: `src/jj.rs:216-252` (remove `get_parents` and `get_candidate_parent`)
- Modify: `tests/e2e.rs` (remove retry tests)

**Step 1: Remove Retry from Commands enum in main.rs**

Delete the `Retry` variant (lines 35-41) and its match arm (line 73).

**Step 2: Remove the retry function from commands.rs**

Delete the entire `pub fn retry(...)` function (lines 294-333).

**Step 3: Remove get_candidate_parent from jj.rs**

Delete `get_candidate_parent` (lines 235-252) and `get_parents` (lines 217-232). These are now unused.

**Step 4: Remove retry tests from e2e.rs**

Delete these tests:
- `test_retry_basic` (lines 699-729)
- `test_retry_with_revset` (lines 731-760)
- `test_retry_not_found` (lines 762-769)

**Step 5: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All remaining tests pass.

**Step 6: Commit**

```
jj desc -m "Remove retry command (replaced by idempotent push)"
```

---

## Task 4: Switch Push Preflight to Headless Merge Commit

The bash script now uses `jj new --no-edit` instead of creating a temporary workspace for the preflight conflict check. This is simpler and faster.

**Files:**
- Modify: `src/jj.rs` (add `abandon` function)
- Modify: `src/commands.rs:25-76` (rewrite preflight in push)

**Step 1: Add jj::abandon function to jj.rs**

```rust
/// Abandon a revision.
pub fn abandon(rev: &str) -> Result<()> {
    run_quiet(&["abandon", rev])
}
```

**Step 2: Rewrite the preflight conflict check in push()**

Replace the workspace-based preflight (lines 38-61 in commands.rs) with:

```rust
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
    bail!(ExitError::new(exit_codes::CONFLICT, "revision conflicts with trunk"));
}
```

This removes the `tempfile` import dependency from the preflight path (though it's still needed for run).

**Step 3: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: `test_push_conflict_detection` passes (snapshot may need update).

**Step 4: Commit**

```
jj desc -m "Switch push preflight to headless merge commit"
```

---

## Task 5: Add Idempotent Push Cleanup

Before queuing, `push` now scans existing queue and failed bookmarks to:
1. Reject exact duplicates (same commit ID) with exit code 10
2. Replace same change ID / different commit ID in queue
3. Clear failed entries whose `jjq-candidate:` trailer matches the change ID

**Files:**
- Modify: `src/jj.rs` (add `resolve_revset_full` to get both change_id and commit_id)
- Modify: `src/commands.rs:25-76` (add cleanup logic to push)

**Step 1: Add resolve_revset_full to jj.rs**

```rust
/// Resolve a revset to both change ID and commit ID.
pub fn resolve_revset_full(revset: &str) -> Result<(String, String)> {
    let output = run(&[
        "log", "-r", revset, "--no-graph", "-T",
        "change_id.short() ++ \" \" ++ commit_id",
    ])?;
    if !output.status.success() {
        bail!("revset '{}' not found", revset);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    if line.is_empty() {
        bail!("revset '{}' not found", revset);
    }
    if line.contains('\n') {
        bail!("revset '{}' resolves to multiple revisions", revset);
    }
    let (change_id, commit_id) = line.split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("unexpected output format from jj log"))?;
    Ok((change_id.to_string(), commit_id.to_string()))
}
```

**Step 2: Add get_description to jj.rs**

```rust
/// Get the full description of a revision.
pub fn get_description(revset: &str) -> Result<String> {
    run_ok(&["log", "-r", revset, "--no-graph", "-T", "description"])
        .map(|s| s.to_string())
}
```

**Step 3: Add idempotent cleanup to push() in commands.rs**

After resolving the revset and before the preflight check, add:

```rust
let (change_id, commit_id) = jj::resolve_revset_full(revset)?;

let trunk_bookmark = config::get_trunk_bookmark()?;

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
    if let Some(candidate_change_id) = extract_candidate_trailer(&desc) {
        if candidate_change_id == change_id {
            let entry_id = extract_id_from_bookmark(bookmark);
            jj::bookmark_delete(bookmark)?;
            prefout(&format!("clearing failed entry {}", entry_id));
        }
    }
}
```

Add these helper functions in commands.rs:

```rust
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
```

**Step 4: Write tests for idempotent push**

Add to e2e.rs:

```rust
#[test]
fn test_push_exact_duplicate_rejected() {
    let repo = TestRepo::with_go_project();

    repo.jjq_success(&["push", "main"]);

    // Same commit ID should be rejected
    let output = repo.jjq_output(&["push", "main"]);
    assert!(output.contains("already queued"));
}

#[test]
fn test_push_idempotent_clears_failed() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    // Create and push a branch
    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("fail.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    // Run to create failed item
    repo.jjq_failure(&["run"]);

    // Verify failed item exists
    let status = repo.jjq_success(&["status"]);
    assert!(status.contains("Failed"));

    // Re-push same change (after amending) should clear the failed entry
    run_jj(repo.path(), &["edit", "fb"]);
    fs::write(repo.path().join("fail.txt"), "fixed content").unwrap();
    run_jj(repo.path(), &["bookmark", "set", "fb"]);

    let repush = repo.jjq_success(&["push", "fb"]);
    assert!(repush.contains("clearing failed entry"));
}
```

**Step 5: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All tests pass including new idempotent push tests.

**Step 6: Commit**

```
jj desc -m "Add idempotent push with queue/failed cleanup"
```

---

## Task 6: Add jjq-candidate Trailer and Resolution Guidance to run_one

When a run fails (conflict or check failure), the bash script now:
1. Captures the candidate's change ID before creating the merge workspace
2. Includes `jjq-candidate: <change-id>` trailer in failure descriptions
3. Outputs actionable "To resolve:" guidance

**Files:**
- Modify: `src/commands.rs:121-245` (update run_one)

**Step 1: Capture candidate change ID before workspace creation**

Before the `jj::workspace_add` call in `run_one()`, add:

```rust
let candidate_change_id = jj::resolve_revset(&format!("bookmarks(exact:{})", queue_bookmark))?;
```

**Step 2: Update conflict failure path**

Change the conflict failure description and messages:

```rust
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
```

**Step 3: Update check failure path**

Similarly update the check failure path:

```rust
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
    return Ok(RunResult::Failure(exit_codes::CHECK_FAILED, format!("merge {} check failed", id)));
}
```

**Step 4: Update trunk moved failure path**

```rust
preferr("trunk bookmark moved during run; queue item left in place, re-run to retry");
return Ok(RunResult::Failure(exit_codes::TRUNK_MOVED, "trunk moved during run".to_string()));
```

**Step 5: Update lock held failure path**

```rust
None => {
    preferr("queue runner lock already held");
    return Ok(RunResult::Failure(exit_codes::LOCK_HELD, "run lock unavailable".to_string()));
}
```

**Step 6: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: Snapshot tests for run failures need updating. Update them.

**Step 7: Commit**

```
jj desc -m "Add jjq-candidate trailer and resolution guidance to failures"
```

---

## Task 7: Make run --all Continue Past Failures

The bash script's `run --all` now continues past conflict and check failures, only bailing immediately on lock-held. It tracks both `merged_count` and `failed_count`.

**Files:**
- Modify: `src/commands.rs:97-119` (rewrite run_all)
- Modify: `src/commands.rs:79-89` (update run to handle exit codes)

**Step 1: Rewrite run_all()**

```rust
fn run_all() -> Result<()> {
    let mut merged_count = 0u32;
    let mut failed_count = 0u32;
    let mut first_failure: Option<i32> = None;

    loop {
        match run_one()? {
            RunResult::Success => {
                merged_count += 1;
            }
            RunResult::Empty => {
                break;
            }
            RunResult::Failure(code, _msg) => {
                if code == exit_codes::LOCK_HELD {
                    // Can't process anything while another runner is active
                    if merged_count > 0 || failed_count > 0 {
                        prefout(&format!(
                            "processed {} item(s), {} failed (lock held, stopping)",
                            merged_count, failed_count
                        ));
                    }
                    return Err(ExitError::new(exit_codes::LOCK_HELD, "run lock unavailable").into());
                }
                // Conflict or check failure — skip and continue
                failed_count += 1;
                if first_failure.is_none() {
                    first_failure = Some(code);
                }
            }
        }
    }

    let total = merged_count + failed_count;
    if total > 0 {
        if failed_count == 0 {
            prefout(&format!("processed {} item(s)", merged_count));
        } else {
            prefout(&format!("processed {} item(s), {} failed", merged_count, failed_count));
        }
    }

    if let Some(code) = first_failure {
        return Err(ExitError::new(code, "one or more items failed").into());
    }
    Ok(())
}
```

**Step 2: Update run() to pass through exit codes**

```rust
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
```

**Step 3: Write test for run --all continuing past failures**

Add to e2e.rs:

```rust
#[test]
fn test_run_all_continues_past_failures() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    // Create a branch that will conflict after first merge
    // f1: clean merge
    run_jj(repo.path(), &["new", "-m", "feature 1", "main"]);
    fs::write(repo.path().join("f1.txt"), "feature 1").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);

    // f2: will conflict with f1 because both modify main.go
    run_jj(repo.path(), &["new", "-m", "feature 2", "main"]);
    fs::write(
        repo.path().join("main.go"),
        "package main\n\nfunc main() {}\n",
    )
    .unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f2"]);

    // f3: clean merge (just adds a file)
    run_jj(repo.path(), &["new", "-m", "feature 3", "main"]);
    fs::write(repo.path().join("f3.txt"), "feature 3").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f3"]);

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // run --all should process f1, fail on f2 (conflict), continue to f3
    let output = repo.jjq_output(&["run", "--all"]);
    assert!(output.contains("merged 1 to main"));
    assert!(output.contains("merge 2 has conflicts"));
    assert!(output.contains("merged 3 to main"));
    assert!(output.contains("2 item(s), 1 failed"));
}
```

**Step 4: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All tests pass, including new run --all resilience test.

**Step 5: Commit**

```
jj desc -m "run --all: continue past failures, bail on lock held"
```

---

## Task 8: Update delete to Clean Up Workspaces

When deleting a failed item, the bash script now also forgets the associated `jjq-run-*` workspace and removes its directory.

**Files:**
- Modify: `src/jj.rs` (add `workspace_list` function)
- Modify: `src/commands.rs:335-356` (update delete)

**Step 1: Add workspace_list to jj.rs**

```rust
/// List all workspace names.
pub fn workspace_list() -> Result<String> {
    run_ok(&["workspace", "list"])
}
```

**Step 2: Add lookup_workspace_path to commands.rs**

This searches jjq metadata log history for workspace paths:

```rust
/// Look up the filesystem path of a workspace from jjq metadata log history.
fn lookup_workspace_path(id: u32) -> Result<Option<String>> {
    let output = jj::run_ok(&[
        "log",
        "-r",
        &format!("ancestors(bookmarks(exact:\"jjq/_/_\"), 100)"),
        "--no-graph",
        "-T",
        "description ++ \"\\n---\\n\"",
    ]);
    match output {
        Ok(text) => {
            let needle = format!("Sequence-Id: {}", id);
            // Find the block containing this sequence ID and extract Workspace line
            for block in text.split("\n---\n") {
                if block.contains(&needle) {
                    for line in block.lines() {
                        if let Some(path) = line.strip_prefix("Workspace: ") {
                            return Ok(Some(path.trim().to_string()));
                        }
                    }
                }
            }
            Ok(None)
        }
        Err(_) => Ok(None),
    }
}
```

**Step 3: Update delete() for failed item workspace cleanup**

In the failed item branch of `delete()`, add workspace cleanup:

```rust
if queue::failed_item_exists(id)? {
    let padded = queue::format_seq_id(id);
    let run_name = format!("jjq-run-{}", padded);

    // Look up workspace path before deleting
    let workspace_path = lookup_workspace_path(id)?;

    jj::bookmark_delete(&queue::failed_bookmark(id))?;
    prefout(&format!("deleted failed item {}", id_str));

    // Try to forget the workspace (silently ignore if not found)
    let _ = jj::workspace_forget(&run_name);

    // Remove directory if found and still exists
    if let Some(ref path) = workspace_path {
        let p = std::path::Path::new(path);
        if p.is_dir() {
            let _ = fs::remove_dir_all(p);
            prefout(&format!("removed workspace {}", path));
        }
    }

    return Ok(());
}
```

**Step 4: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All tests pass.

**Step 5: Commit**

```
jj desc -m "delete: clean up workspace when removing failed items"
```

---

## Task 9: Add the clean Command

The `clean` command enumerates all `jjq-run-*` workspaces, labels them, forgets them, and removes their directories.

**Files:**
- Modify: `src/main.rs` (add Clean variant to Commands enum)
- Modify: `src/commands.rs` (add clean function)
- Modify: `tests/e2e.rs` (add clean test)

**Step 1: Add Clean to Commands enum in main.rs**

```rust
/// Remove jjq workspaces
Clean,
```

And add the match arm:

```rust
Commands::Clean => commands::clean(),
```

**Step 2: Implement the clean function in commands.rs**

```rust
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
        let workspace_path = lookup_workspace_path(plain_id)?;

        // Forget the workspace
        let _ = jj::workspace_forget(ws_name);

        // Remove directory if found
        if let Some(ref path) = workspace_path {
            let p = std::path::Path::new(path);
            if p.is_dir() {
                let _ = fs::remove_dir_all(p);
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
```

**Step 3: Write test for clean**

Add to e2e.rs:

```rust
#[test]
fn test_clean_no_workspaces() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_success(&["clean"]);
    insta::assert_snapshot!(output, @"jjq: no workspaces to clean");
}

#[test]
fn test_clean_removes_failed_workspaces() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("fail.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    // Run to create failed item (and preserved workspace)
    repo.jjq_failure(&["run"]);

    // Clean should find and remove the workspace
    let output = repo.jjq_success(&["clean"]);
    assert!(output.contains("removed 1 workspace"));
}
```

**Step 4: Build and test**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All tests pass.

**Step 5: Commit**

```
jj desc -m "Add clean command to remove jjq workspaces"
```

---

## Task 10: Update Existing Tests and Fix Snapshot Mismatches

At this point, many existing snapshot tests will have stale expectations due to the changes above (new error messages, removed retry, changed failure output with guidance, etc.).

**Files:**
- Modify: `tests/e2e.rs`

**Step 1: Run all tests and identify failures**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`

**Step 2: Update snapshots**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo insta review`

Review each snapshot diff carefully. The key changes expected:
- `test_run_check_failure`: Now includes "To resolve:" guidance and `jjq-candidate:` in the description visible in status
- `test_full_workflow_with_prs`: Conflict failure now shows guidance
- `test_push_conflict_detection`: Exit code changes may affect output
- `test_multiple_push_same_revision`: Now rejected as duplicate (same commit ID)

For `test_multiple_push_same_revision`: The bash script would reject the second push as "already queued" since it's the exact same commit ID. The test needs updating — either change it to expect failure, or change the test to push different commit IDs for the same change.

**Step 3: Build and test again**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All tests pass.

**Step 4: Commit**

```
jj desc -m "Update test snapshots for new behaviors"
```

---

## Task 11: Final Verification and Cleanup

**Step 1: Run the full test suite**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo test`
Expected: All tests pass.

**Step 2: Run clippy**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo clippy`
Expected: No warnings.

**Step 3: Check for dead code**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo build 2>&1 | grep warning`
Expected: No unused warnings.

**Step 4: Verify the binary works manually (optional)**

Run: `cd /Users/paul/projects/incubator/jjq/.workspaces/rust-impl && cargo run -- status`

**Step 5: Commit any final cleanups**

```
jj desc -m "Final cleanup and verification"
```
