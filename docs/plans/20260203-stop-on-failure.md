# `--stop-on-failure` Flag for `jjq run --all` Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `jjq run --all` continue processing the queue after failures by default, with `--stop-on-failure` to restore the old stop-immediately behavior.

**Architecture:** Modify the `run_all()` loop to skip failed items (moving them to `jjq/failed/` as today) and continue to the next queue item. Track both `merged_count` and `failed_count`. Introduce exit code 2 (`PARTIAL`) for mixed success/failure runs. The `--stop-on-failure` flag preserves the old behavior (stop at first failure, exit code 1).

**Tech Stack:** Rust, clap (CLI parsing), insta (snapshot tests)

---

### Task 1: Add `PARTIAL` exit code constant

**Files:**
- Modify: `src/exit_codes.rs:6-8`

**Step 1: Write the new constant**

Add between `CONFLICT` and `LOCK_HELD`:

```rust
pub const PARTIAL: i32 = 2;
```

So the block reads:

```rust
pub const CONFLICT: i32 = 1;
pub const PARTIAL: i32 = 2;
pub const LOCK_HELD: i32 = 3;
pub const USAGE: i32 = 10;
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: success (constant is unused for now, no warning needed since we'll use it shortly)

**Step 3: Commit**

```
jj commit -m "Add PARTIAL exit code (2) for mixed success/failure runs"
```

---

### Task 2: Add `--stop-on-failure` CLI flag

**Files:**
- Modify: `src/main.rs:28-33` (Run variant)
- Modify: `src/main.rs:71` (match arm)
- Modify: `src/commands.rs:194` (fn signature)

**Step 1: Add the flag to the CLI definition**

In `src/main.rs`, change the `Run` variant from:

```rust
    /// Process the next item(s) in the queue
    Run {
        /// Process all queued items until empty or failure
        #[arg(long)]
        all: bool,
    },
```

to:

```rust
    /// Process the next item(s) in the queue
    Run {
        /// Process all queued items until empty or failure
        #[arg(long)]
        all: bool,
        /// Stop processing on first failure (only with --all)
        #[arg(long)]
        stop_on_failure: bool,
    },
```

**Step 2: Update the match arm in `run()`**

In `src/main.rs`, change:

```rust
        Commands::Run { all } => commands::run(all),
```

to:

```rust
        Commands::Run { all, stop_on_failure } => commands::run(all, stop_on_failure),
```

**Step 3: Update the `commands::run` signature**

In `src/commands.rs`, change:

```rust
pub fn run(all: bool) -> Result<()> {
    if all {
        run_all()
```

to:

```rust
pub fn run(all: bool, stop_on_failure: bool) -> Result<()> {
    if all {
        run_all(stop_on_failure)
```

And update `run_all` signature from `fn run_all()` to `fn run_all(stop_on_failure: bool)` (body unchanged for now — just thread the parameter through):

```rust
fn run_all(stop_on_failure: bool) -> Result<()> {
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: success (warning about unused `stop_on_failure` is fine)

**Step 5: Commit**

```
jj commit -m "Thread --stop-on-failure flag through CLI to run_all"
```

---

### Task 3: Update existing test to match new default behavior

The existing test `test_run_all_stops_on_first_failure` tests the *current* default (stop on failure). After our change, this behavior moves behind `--stop-on-failure`. We need to:

1. Rename this test to `test_run_all_stop_on_failure_flag` and add `--stop-on-failure` to its invocation.
2. Write a new test for the new default (continue on failure).

**Files:**
- Modify: `tests/e2e.rs:845-886` (rename + add flag)

**Step 1: Update the existing test**

In `tests/e2e.rs`, rename `test_run_all_stops_on_first_failure` to `test_run_all_stop_on_failure_flag`. Change the invocation from:

```rust
    let output = repo.jjq_output(&["run", "--all"]);
```

to:

```rust
    let output = repo.jjq_output(&["run", "--all", "--stop-on-failure"]);
```

The assertions stay exactly the same — the behavior under `--stop-on-failure` is identical to the old default.

**Step 2: Run the renamed test**

Run: `cargo test test_run_all_stop_on_failure_flag -- --nocapture`
Expected: PASS (behavior is unchanged, just invoked with the new flag)

**Step 3: Commit**

```
jj commit -m "Rename existing stop-on-failure test to use --stop-on-failure flag"
```

---

### Task 4: Write failing test for continue-on-failure (new default)

**Files:**
- Modify: `tests/e2e.rs` (add new test after the renamed one)

**Step 1: Write the test**

Add this test after `test_run_all_stop_on_failure_flag`:

```rust
#[test]
fn test_run_all_continues_on_failure() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    // f1: modifies main.go (will merge cleanly against trunk)
    run_jj(repo.path(), &["new", "-m", "feature 1", "main"]);
    fs::write(
        repo.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 1\")\n}\n",
    )
    .unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);

    // f2: also modifies main.go differently — will conflict after f1 merges
    run_jj(repo.path(), &["new", "-m", "feature 2", "main"]);
    fs::write(
        repo.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 2\")\n}\n",
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

    // run --all should process f1, fail on f2 (conflict), CONTINUE, and process f3
    let output = repo.jjq_output(&["run", "--all"]);
    assert!(output.contains("merged 1 to main"), "f1 should merge: {}", output);
    assert!(output.contains("merge 2 has conflicts"), "f2 should conflict: {}", output);
    assert!(output.contains("merged 3 to main"), "f3 SHOULD be processed: {}", output);
    assert!(output.contains("processed 2 item(s), 1 failed"), "summary should show mixed results: {}", output);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_run_all_continues_on_failure -- --nocapture`
Expected: FAIL — f3 is not processed (old behavior still in `run_all`)

**Step 3: Commit the failing test**

```
jj commit -m "Add failing test: run --all continues on failure by default"
```

---

### Task 5: Implement continue-on-failure in `run_all`

**Files:**
- Modify: `src/commands.rs:212-236` (`run_all` function)

**Step 1: Rewrite `run_all`**

Replace the entire `run_all` function with:

```rust
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
```

Key behaviors:
- `stop_on_failure == true`: identical to old behavior (stop on first failure, exit 1)
- `stop_on_failure == false` (default): skip failed items, continue loop, exit 2 if any failed
- When nothing was processed at all (empty queue), exits 0 silently (existing `prefout("queue is empty")` happens inside `run_one`)

**Step 2: Run the new test**

Run: `cargo test test_run_all_continues_on_failure -- --nocapture`
Expected: PASS

**Step 3: Run all tests**

Run: `cargo test -- --nocapture`
Expected: all tests PASS

**Step 4: Commit**

```
jj commit -m "Make run --all continue on failure by default"
```

---

### Task 6: Add test for exit code 2 on partial failure

The test in Task 4 uses `jjq_output` which doesn't assert exit codes. We need a test that verifies exit code 2.

**Files:**
- Modify: `tests/e2e.rs` (add new test)

We need a helper to assert a specific exit code. The existing test infrastructure uses `assert_cmd` which supports `.code()` predicate.

**Step 1: Write the exit-code test**

Add this test:

```rust
#[test]
fn test_run_all_partial_failure_exit_code() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    // f1: clean merge
    run_jj(repo.path(), &["new", "-m", "feature 1", "main"]);
    fs::write(repo.path().join("f1.txt"), "feature 1").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);

    // f2: will conflict (modifies main.go)
    run_jj(repo.path(), &["new", "-m", "feature 2", "main"]);
    fs::write(
        repo.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 2\")\n}\n",
    )
    .unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f2"]);

    // f3: also modifies main.go differently — will conflict after f1 merges
    run_jj(repo.path(), &["new", "-m", "feature 3", "main"]);
    fs::write(
        repo.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 3\")\n}\n",
    )
    .unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f3"]);

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // Exit code should be 2 (PARTIAL) — some succeeded, some failed
    repo.jjq()
        .args(["run", "--all"])
        .assert()
        .code(2);
}
```

Note: This uses `repo.jjq()` directly to get access to the `assert_cmd` chain for exit code checking.

**Step 2: Run test**

Run: `cargo test test_run_all_partial_failure_exit_code -- --nocapture`
Expected: PASS

**Step 3: Commit**

```
jj commit -m "Add test: run --all exits 2 on partial failure"
```

---

### Task 7: Update existing `test_run_all` snapshot

The existing `test_run_all` test (all items succeed, no failures) should still pass unchanged — exit 0, same output format. Verify this.

**Step 1: Run the existing test**

Run: `cargo test test_run_all -- --nocapture --exact`
Expected: PASS with no snapshot changes

If the snapshot needs updating (shouldn't, but if so):

Run: `cargo insta review`

**Step 2: Commit only if snapshot changed**

```
jj commit -m "Update test_run_all snapshot for new run_all behavior"
```

---

### Task 8: Update man page and docs

**Files:**
- Modify: `docs/jjq.1.txt:44-63` (run command docs)
- Modify: `docs/jjq.1.txt:104-112` (exit codes section)

**Step 1: Update run command documentation**

In `docs/jjq.1.txt`, replace the `run` command section (lines 44-63) with:

```
     run [--all] [--stop-on-failure]
         Process the next queued item. Creates a temporary jj workspace with
         two parents (trunk and candidate), then runs the configured check
         command inside it.

         On success, the trunk bookmark advances to the merge commit and
         the temporary workspace is cleaned up.

         On failure (conflicts or check command failure), the item moves to
         the failed list. The temporary workspace is preserved for debugging.

         If the trunk bookmark moves during processing (e.g., another runner
         advanced it), the run aborts and instructs the user to retry.

         With --all, processes items in a loop until the queue is empty.
         Failed items are moved to the failed list and processing continues
         with the next item. Reports the count of successes and failures.

         With --all --stop-on-failure, stops at the first failure instead
         of continuing.

         Returns 0 if all items merged successfully (or the queue was empty).
         Returns 1 if --stop-on-failure is set and an item fails. Returns 2
         if some items succeeded and some failed.
```

**Step 2: Update exit codes section**

Replace the exit codes section (lines 104-112) with:

```
  EXIT CODES
         0    Success.

         1    Merge conflict or check command failure.

         2    Partial success. Some items merged, some failed (run --all
              without --stop-on-failure).

         3    Lock held. Another jjq process is running. Retry later.

         10   Usage error. Bad arguments, ambiguous revset, duplicate push
              of an amended revision, etc.
```

**Step 3: Update SYNOPSIS**

Change line 8 from:

```
         jjq run [--all]
```

to:

```
         jjq run [--all] [--stop-on-failure]
```

**Step 4: Commit**

```
jj commit -m "Document --stop-on-failure flag and exit code 2 in man page"
```

---

### Task 9: Final verification

**Step 1: Run full test suite**

Run: `cargo test -- --nocapture`
Expected: all tests PASS

**Step 2: Manual smoke test**

Run: `cargo build && ./target/debug/jjq run --help`
Expected: output shows both `--all` and `--stop-on-failure` flags with descriptions

**Step 3: Verify clippy is clean**

Run: `cargo clippy -- -D warnings`
Expected: no warnings

**Step 4: Final commit if any fixups needed, then squash**

Review the series of commits and squash if Paul prefers a single commit.
