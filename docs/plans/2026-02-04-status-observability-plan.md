# Status & Observability Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make jjq's queue state machine-readable (`status --json`, single-item filters) and enrich failed item metadata with trailers.

**Architecture:** Extend the existing `status` command with `--json` flag and optional ID/`--resolve` filters. Enrich failed item commit descriptions with additional trailers at the two failure points in `run_one()`. Add `serde`/`serde_json` for JSON serialization. Extract a generic trailer parser from the existing `extract_candidate_trailer`.

**Tech Stack:** Rust, clap (CLI), serde + serde_json (JSON output)

**Design doc:** `docs/plans/2026-02-04-status-observability-design.md`

---

### Task 1: Add serde/serde_json dependency

**Files:**
- Modify: `Cargo.toml`

**Step 1: Add dependencies**

Add `serde` and `serde_json` to `[dependencies]` in `Cargo.toml`:

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: compiles successfully

**Step 3: Commit**

```
jj desc -m "Add serde and serde_json dependencies"
jj new
```

---

### Task 2: Extract generic trailer parser

Replace the single-purpose `extract_candidate_trailer` with a general `extract_trailers` function that parses all `jjq-*` trailers from a commit description into a `HashMap`.

**Files:**
- Modify: `src/commands.rs:35-43` (replace `extract_candidate_trailer`)

**Step 1: Write `extract_trailers`**

Replace the existing `extract_candidate_trailer` function (lines 35-43) with:

```rust
use std::collections::HashMap;

/// Extract all jjq-* trailers from a commit description into a map.
/// Trailer format: "jjq-key: value" — returns map of "key" -> "value".
fn extract_trailers(description: &str) -> HashMap<String, String> {
    let mut trailers = HashMap::new();
    for line in description.lines() {
        if let Some(rest) = line.strip_prefix("jjq-") {
            if let Some((key, value)) = rest.split_once(": ") {
                trailers.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    trailers
}
```

**Step 2: Update call site in `push()`**

In `push()` (around line 152), replace:
```rust
if let Some(candidate_change_id) = extract_candidate_trailer(&desc)
    && candidate_change_id == change_id
```
with:
```rust
let trailers = extract_trailers(&desc);
if let Some(candidate_change_id) = trailers.get("candidate")
    && *candidate_change_id == change_id
```

**Step 3: Verify it compiles and the existing logic is preserved**

Run: `cargo build`
Expected: compiles successfully

**Step 4: Commit**

```
jj desc -m "Extract generic trailer parser from extract_candidate_trailer"
jj new
```

---

### Task 3: Enrich failed item trailers in `run_one()`

Add the new trailers (`jjq-candidate-commit`, `jjq-trunk`, `jjq-workspace`, `jjq-failure`) to both failure points in `run_one()`.

**Files:**
- Modify: `src/commands.rs:287-366` (the `run_one` function, both failure paths)

**Step 1: Capture candidate commit ID**

At line 292, change:
```rust
let candidate_change_id = jj::resolve_revset(&format!("bookmarks(exact:{})", queue_bookmark))?;
```
to:
```rust
let (candidate_change_id, candidate_commit_id) = jj::resolve_revset_full(&format!("bookmarks(exact:{})", queue_bookmark))?;
```

**Step 2: Update the conflict failure description (around line 318-321)**

Change:
```rust
jj::describe(
    &workspace_rev,
    &format!("Failed: merge {} (conflicts)\n\njjq-candidate: {}", id, candidate_change_id),
)?;
```
to:
```rust
jj::describe(
    &workspace_rev,
    &format!(
        "Failed: merge {} (conflicts)\n\njjq-candidate: {}\njjq-candidate-commit: {}\njjq-trunk: {}\njjq-workspace: {}\njjq-failure: conflicts",
        id, candidate_change_id, candidate_commit_id, trunk_commit_id,
        runner_workspace.path().display()
    ),
)?;
```

Note: `runner_workspace.path()` must be read **before** `runner_workspace.keep()` is called (which consumes it). The describe happens before `keep()`, so this is safe.

**Step 3: Update the check failure description (around line 350-353)**

Change:
```rust
jj::describe(
    &workspace_rev,
    &format!("Failed: merge {} (check)\n\njjq-candidate: {}", id, candidate_change_id),
)?;
```
to:
```rust
jj::describe(
    &workspace_rev,
    &format!(
        "Failed: merge {} (check)\n\njjq-candidate: {}\njjq-candidate-commit: {}\njjq-trunk: {}\njjq-workspace: {}\njjq-failure: check",
        id, candidate_change_id, candidate_commit_id, trunk_commit_id,
        runner_workspace.path().display()
    ),
)?;
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: compiles successfully

**Step 5: Commit**

```
jj desc -m "Write enriched trailers on failed items"
jj new
```

---

### Task 4: Update CLI definition for `status` command

Add `--json`, optional positional `id`, and `--resolve` to the `Status` variant.

**Files:**
- Modify: `src/main.rs:47` (the `Status` variant in `Commands` enum)
- Modify: `src/main.rs:87` (the match arm)

**Step 1: Update the `Status` variant**

Replace:
```rust
/// Display current queue state
Status,
```
with:
```rust
/// Display current queue state
Status {
    /// Sequence ID of a specific item to show
    id: Option<String>,
    /// Output as JSON
    #[arg(long)]
    json: bool,
    /// Look up item by candidate change ID
    #[arg(long, conflicts_with = "id")]
    resolve: Option<String>,
},
```

**Step 2: Update the match arm**

Replace:
```rust
Commands::Status => commands::status(),
```
with:
```rust
Commands::Status { id, json, resolve } => commands::status(id.as_deref(), json, resolve.as_deref()),
```

**Step 3: Update the `status` function signature**

In `src/commands.rs`, change:
```rust
pub fn status() -> Result<()> {
```
to:
```rust
pub fn status(id: Option<&str>, json: bool, resolve: Option<&str>) -> Result<()> {
```

For now, ignore the new parameters (leave the body unchanged) so it compiles.

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: compiles successfully. `jjq status` still works. `jjq status --help` shows new flags.

**Step 5: Commit**

```
jj desc -m "Add --json, id, and --resolve args to status command"
jj new
```

---

### Task 5: Define JSON output structs

Define the serializable structs for `--json` output.

**Files:**
- Modify: `src/commands.rs` (add structs near top of file, after imports)

**Step 1: Add serde imports and structs**

After the existing `use` block at the top of `commands.rs`, add:

```rust
use serde::Serialize;

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
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: compiles (structs unused for now, may get warnings — that's fine)

**Step 3: Commit**

```
jj desc -m "Define JSON output structs for status command"
jj new
```

---

### Task 6: Add helper to build queue/failed item data

Add functions that build `QueueItem` and `FailedItem` from bookmark data. These are used by both the overview and single-item modes.

**Files:**
- Modify: `src/commands.rs`

**Step 1: Add `build_queue_item` helper**

```rust
/// Build a QueueItem by resolving data from the bookmark target.
fn build_queue_item(id: u32) -> Result<QueueItem> {
    let bookmark = queue::queue_bookmark(id);
    let revset = format!("bookmarks(exact:{})", bookmark);
    let (change_id, commit_id) = jj::resolve_revset_full(&revset)?;
    let description = jj::get_description(&revset)?;
    let description = description.lines().next().unwrap_or("").to_string();
    Ok(QueueItem { id, change_id, commit_id, description })
}
```

**Step 2: Add `build_failed_item` helper**

```rust
/// Build a FailedItem by parsing trailers from the bookmark target description.
fn build_failed_item(id: u32) -> Result<FailedItem> {
    let bookmark = queue::failed_bookmark(id);
    let revset = format!("bookmarks(exact:{})", bookmark);
    let desc = jj::get_description(&revset)?;
    let trailers = extract_trailers(&desc);

    let candidate_change_id = trailers.get("candidate").cloned().unwrap_or_default();
    let candidate_commit_id = trailers.get("candidate-commit").cloned().unwrap_or_default();
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
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: compiles successfully

**Step 4: Commit**

```
jj desc -m "Add helpers to build QueueItem and FailedItem from bookmarks"
jj new
```

---

### Task 7: Implement `status --json` (overview mode)

Rewrite the `status` function body to handle the `json` flag for overview mode (no ID or resolve filter).

**Files:**
- Modify: `src/commands.rs` (the `status` function body)

**Step 1: Implement the overview JSON path**

Rewrite `status()` to handle both text and JSON overview. The function should:

1. Early-return on not-initialized (text: current message; JSON: `{"running":false,"queue":[],"failed":[]}`)
2. Gather data: `running` from `lock::is_held("run")`, queue items via `build_queue_item`, failed items via `build_failed_item` (limited by `max_failures`)
3. If `json` and no ID/resolve: serialize `StatusOutput` and print
4. If not `json` and no ID/resolve: print current text format but using the new helpers

For now, only handle the case where `id.is_none() && resolve.is_none()` (overview mode). Add a `bail!("not yet implemented")` for the filtered modes — they'll be Task 8.

Here is the full replacement body for `status()`:

```rust
pub fn status(id: Option<&str>, json: bool, resolve: Option<&str>) -> Result<()> {
    // Single-item modes (Task 8)
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
            prefout("jjq not initialized (run 'jjq push <revset>' to start)");
        }
        return Ok(());
    }

    let _config_lock = Lock::acquire_or_fail("config", "config lock unavailable")?;
    let max_failures = config::get_max_failures()?;
    drop(_config_lock);

    let running = lock::is_held("run")?;

    let queue_ids = queue::get_queue()?;
    let failed_ids = queue::get_failed()?;

    let queue_items: Vec<QueueItem> = queue_ids
        .iter()
        .map(|&id| build_queue_item(id))
        .collect::<Result<_>>()?;

    let failed_items: Vec<FailedItem> = failed_ids
        .iter()
        .take(max_failures as usize)
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
                println!("  {}: {} {}", item.id, item.candidate_change_id, item.description);
            }
        }
    }

    Ok(())
}
```

**Step 2: Add the `status_single` stub**

```rust
fn status_single(_id: Option<&str>, _json: bool, _resolve: Option<&str>) -> Result<()> {
    bail!("single-item status not yet implemented")
}
```

**Step 3: Add the serde_json import**

At the top of the file, no new import needed — serde_json is used directly as `serde_json::to_string_pretty`. But make sure `use serde::Serialize;` is present (added in Task 5).

**Step 4: Verify it compiles and text mode still works**

Run: `cargo build`
Expected: compiles successfully

**Step 5: Commit**

```
jj desc -m "Implement status --json for overview mode"
jj new
```

---

### Task 8: Implement single-item status (`status <id>` and `--resolve`)

**Files:**
- Modify: `src/commands.rs` (replace `status_single` stub)

**Step 1: Implement `status_single`**

Replace the stub with the full implementation:

```rust
fn status_single(id: Option<&str>, json: bool, resolve: Option<&str>) -> Result<()> {
    // Resolve the item — either by sequence ID or by candidate change ID
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
            println!("  Candidate:   {} ({})", item.candidate_change_id, item.candidate_commit_id);
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
```

**Step 2: Implement `find_by_change_id`**

```rust
/// Find a queue or failed item by candidate change ID.
/// Returns (sequence_id, is_queued).
fn find_by_change_id(change_id: &str) -> Result<(u32, bool)> {
    // Search queue items
    let queue_ids = queue::get_queue()?;
    for id in &queue_ids {
        let bookmark = queue::queue_bookmark(*id);
        let revset = format!("bookmarks(exact:{})", bookmark);
        if let Ok(item_change_id) = jj::resolve_revset(&revset) {
            if item_change_id == change_id {
                return Ok((*id, true));
            }
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
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: compiles successfully

**Step 4: Commit**

```
jj desc -m "Implement single-item status with --resolve support"
jj new
```

---

### Task 9: Add e2e tests for new status features

Add tests to `tests/e2e.rs` exercising the new status features. Use the existing `TestRepo` harness and `insta` snapshots. Also update existing snapshot tests whose output changes due to the enriched trailer format (the `test_run_check_failure` status snapshot now shows the candidate's original description instead of the jjq failure message).

**Files:**
- Modify: `tests/e2e.rs`

**Step 1: Update existing `test_run_check_failure` snapshot**

The status output for failed items now resolves the original candidate description. Update the snapshot in `test_run_check_failure` where it checks status after failure — the failed item line should now show the candidate's description ("will fail check") instead of the jjq failure message ("Failed: merge 1 (check)").

**Step 2: Add `test_status_json_empty`**

Test that `status --json` returns valid JSON for an uninitialized repo:

```rust
#[test]
fn test_status_json_empty() {
    let repo = TestRepo::new();
    let output = repo.jjq_success(&["status", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    assert_eq!(parsed["running"], false);
    assert_eq!(parsed["queue"], serde_json::json!([]));
    assert_eq!(parsed["failed"], serde_json::json!([]));
}
```

**Step 3: Add `test_status_json_with_items`**

Test JSON output with queued and failed items:

```rust
#[test]
fn test_status_json_with_items() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    // Create and push two branches
    run_jj(repo.path(), &["new", "-m", "queued item", "main"]);
    fs::write(repo.path().join("q.txt"), "queued").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "qb"]);
    repo.jjq_success(&["push", "qb"]);

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("f.txt"), "fail").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    // Run to fail second item (first one fails too since check is "false")
    repo.jjq_output(&["run"]);  // fails item 1
    // Re-push item 1 with passing check and process
    repo.jjq_success(&["config", "check_command", "true"]);
    run_jj(repo.path(), &["new", "-m", "queued item 2", "main"]);
    fs::write(repo.path().join("q2.txt"), "queued2").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "qb2"]);
    repo.jjq_success(&["push", "qb2"]);

    let output = repo.jjq_success(&["status", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");

    // Verify structure
    assert_eq!(parsed["running"], false);
    assert!(parsed["queue"].is_array());
    assert!(parsed["failed"].is_array());

    // Queue items should have change_id, commit_id, description
    for item in parsed["queue"].as_array().unwrap() {
        assert!(item["id"].is_u64());
        assert!(item["change_id"].is_string());
        assert!(item["commit_id"].is_string());
        assert!(item["description"].is_string());
    }

    // Failed items should have all trailer fields
    for item in parsed["failed"].as_array().unwrap() {
        assert!(item["id"].is_u64());
        assert!(item["candidate_change_id"].is_string());
        assert!(item["failure_reason"].is_string());
    }
}
```

**Step 4: Add `test_status_single_item`**

Test `status <id>` for both queued and failed items:

```rust
#[test]
fn test_status_single_item() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("f.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    // Check queued item detail view
    let output = repo.jjq_success(&["status", "1"]);
    assert!(output.contains("Queue item 1"));
    assert!(output.contains("Change ID:"));
    assert!(output.contains("Commit ID:"));
    assert!(output.contains("test feature"));
}
```

**Step 5: Add `test_status_single_failed_item`**

```rust
#[test]
fn test_status_single_failed_item() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("f.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);
    repo.jjq_failure(&["run"]);

    let output = repo.jjq_success(&["status", "1"]);
    assert!(output.contains("Failed item 1"));
    assert!(output.contains("Candidate:"));
    assert!(output.contains("Failure:"));
    assert!(output.contains("check"));
    assert!(output.contains("Trunk:"));
    assert!(output.contains("Workspace:"));
    assert!(output.contains("To resolve:"));
}
```

**Step 6: Add `test_status_single_json`**

```rust
#[test]
fn test_status_single_json() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    run_jj(repo.path(), &["new", "-m", "json test", "main"]);
    fs::write(repo.path().join("f.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    let output = repo.jjq_success(&["status", "1", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    assert_eq!(parsed["id"], 1);
    assert!(parsed["change_id"].is_string());
    assert!(parsed["description"].as_str().unwrap().contains("json test"));
}
```

**Step 7: Add `test_status_not_found`**

```rust
#[test]
fn test_status_not_found() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["push", "main"]);

    let output = repo.jjq_failure(&["status", "999"]);
    assert!(output.contains("item 999 not found"));
}
```

**Step 8: Add `serde_json` to `[dev-dependencies]`**

In `Cargo.toml`, add to `[dev-dependencies]`:

```toml
serde_json = "1"
```

**Step 9: Run the tests**

Run: `cargo test`
Expected: all tests pass, including updated snapshots

**Step 10: Commit**

```
jj desc -m "Add e2e tests for status --json and single-item filters"
jj new
```

---

### Task 10: Run `jjq-test` e2e script

Run the bash-based end-to-end test script to verify nothing is broken.

**Step 1: Build the project**

```bash
cargo build
```

**Step 2: Run the e2e test script**

```bash
JJQ_BIN=./target/debug/jjq ./jjq-test
```

Expected: the full e2e test passes (generate, run, verify). The enriched
trailers don't affect the bash test's conflict resolution flow because it
parses the description first line, which is unchanged ("Failed: merge N
(conflicts)").

**Step 3: Fix any issues and commit**

```
jj desc -m "Fix issues found during e2e testing"
jj new
```

---

### Task 11: Update documentation

Update all docs to reflect the new `status` command features.

**Files:**
- Modify: `README.md` (usage section for status)
- Modify: `docs/jjq.1` (man page roff source — status section)
- Modify: `docs/jjq.1.txt` (plaintext man page — status section)
- Modify: `docs/README.md` (specification — Status section)
- Modify: `AGENTS.md` (commands table for status)

**Step 1: Update `README.md`**

In the "Check status" section, expand to show the new features:

```markdown
### Check status

```sh
jjq status                          # overview of queue and recent failures
jjq status --json                   # machine-readable JSON output
jjq status 42                       # detail view of item 42
jjq status 42 --json                # detail view as JSON
jjq status --resolve <change_id>    # look up item by candidate change ID
```
```

**Step 2: Update `docs/jjq.1` (man page roff source)**

Replace the `.SS status` section with:

```roff
.SS status \fR[\fIid\fR] [\fB\-\-json\fR] [\fB\-\-resolve \fIchange_id\fR]
Display the current queue state: queued items (ascending by sequence ID)
and recent failures (descending, up to
.BR max_failures ).
Each entry shows its sequence ID, change ID prefix, and first line of
the commit description.
.PP
Also indicates if a run lock is currently held (another
.B jjq run
is in progress).
.PP
With
.BR \-\-json ,
outputs structured JSON with
.BR running ,
.BR queue ,
and
.B failed
fields.
Queue items include
.BR change_id ,
.BR commit_id ,
and
.BR description .
Failed items include
.BR candidate_change_id ,
.BR candidate_commit_id ,
.BR description
(original candidate message),
.BR trunk_commit_id ,
.BR workspace_path ,
and
.BR failure_reason .
.PP
With a positional
.IR id ,
displays a single item's detail view (from either queue or failed).
With
.BR \-\-resolve ,
looks up an item by its candidate change ID.
.I id
and
.B \-\-resolve
are mutually exclusive.
```

**Step 3: Update `docs/jjq.1.txt`**

Regenerate from the roff source or manually update the plaintext status
section to match.

**Step 4: Update the synopsis in `docs/jjq.1`**

Change:
```roff
.B jjq status
```
to:
```roff
.B jjq status
.RI [ id ]
.RB [ \-\-json ]
.RB [ \-\-resolve
.IR change_id ]
```

**Step 5: Update `docs/README.md` (specification)**

In the Status section (around line 202-213), add a paragraph about
`--json` output and single-item filters.

**Step 6: Update `AGENTS.md`**

In the commands table, change:
```
| `status` | Show queue and recent failures |
```
to:
```
| `status [id] [--json] [--resolve]` | Show queue and recent failures; supports JSON output and single-item detail view |
```

**Step 7: Commit**

```
jj desc -m "Update docs for status --json and single-item filters"
jj new
```

---

### Task 12: Squash and finalize

Squash the implementation commits into clean, logical units.

**Step 1: Review the commit history**

```
jj log
```

**Step 2: Squash into logical commits**

Aim for 2-3 commits:
1. "Add enriched trailers on failed items" (Tasks 2-3)
2. "Add status --json and single-item filters" (Tasks 1, 4-8, 9-11)

Or squash everything into a single commit if it reads cleaner.

**Step 3: Final compile and verify**

```bash
cargo build
cargo clippy
cargo test
JJQ_BIN=./target/debug/jjq ./jjq-test
```
