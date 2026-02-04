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

### Task 9: Manual integration test

Verify the full workflow end-to-end in a scratch jj repo.

**Step 1: Create a test repo and run through the workflow**

```bash
cd /tmp && mkdir jjq-test && cd jjq-test
jj git init --colocate
echo "hello" > file.txt
jj desc -m "Initial commit"
jj bookmark create main
jj new -m "Test change 1"
echo "change1" > change1.txt

# Configure and push
jjq config check_command "true"
jjq push @
jjq status
jjq status --json

# Run it (should succeed)
jjq run
jjq status
jjq status --json

# Create a failing change
jj new -m "Test change 2"
echo "change2" > change2.txt
jjq config check_command "false"
jjq push @
jjq run

# Now check status with failed items
jjq status
jjq status --json
jjq status 3           # single item by ID
jjq status 3 --json    # single item JSON
```

Expected:
- `jjq status` text output shows queue and failed items with readable format
- `jjq status --json` produces valid JSON with all fields populated
- `jjq status 3` shows the failed item detail view with all trailer data
- `jjq status 3 --json` shows single-item JSON
- Failed items have `candidate_change_id`, `candidate_commit_id`, `trunk_commit_id`, `workspace_path`, and `failure_reason` populated

**Step 2: Test `--resolve`**

```bash
# Use the candidate change ID from the failed item
jjq status --resolve <change_id_from_above>
jjq status --resolve <change_id_from_above> --json
```

**Step 3: Test edge cases**

```bash
# Empty queue
jjq status --json   # should return {"running":false,"queue":[],"failed":[]}

# Item not found
jjq status 999      # should error with "item 999 not found"

# Mutually exclusive args
jjq status 1 --resolve abc  # should error from clap
```

**Step 4: Clean up**

```bash
cd /tmp && rm -rf jjq-test
```

**Step 5: Commit any fixes found during testing**

```
jj desc -m "Fix issues found during integration testing"
jj new
```

---

### Task 10: Squash and finalize

Squash the implementation commits into clean, logical units.

**Step 1: Review the commit history**

```
jj log
```

**Step 2: Squash into logical commits**

Aim for 2-3 commits:
1. "Add enriched trailers on failed items" (Tasks 2-3)
2. "Add status --json and single-item filters" (Tasks 1, 4-8)

Or squash everything into a single commit if it reads cleaner.

**Step 3: Final compile and verify**

```bash
cargo build
cargo clippy
```
