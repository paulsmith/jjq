# Conflict Checks, Exit Codes, and Run --all Resilience Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make jjq fail fast on conflicts (retry pre-flight check), continue past failures in batch mode (run --all), and return distinct exit codes for programmatic use.

**Architecture:** All changes are in the single `jjq` bash script. Extract a shared conflict-check helper, define exit code constants at the top of the file, update all return/exit sites, then update the run --all loop. SPEC.md and jjq-test get updated to match.

**Tech Stack:** Bash, jj CLI

---

### Task 1: Extract pre-flight conflict check into shared helper

The conflict check logic exists in `cmd_push` (jjq:342-366). Extract it into a reusable function so both push and retry can call it.

**Files:**
- Modify: `jjq:342-366` (cmd_push conflict check → call helper)
- Modify: `jjq:8-18` area (add new helper function near other helpers)

**Step 1: Write the helper function**

Add `preflight_conflict_check` after the `get_max_failures` function (after line 169), before `validate_seq_id`. This function takes a revset and a trunk bookmark, creates a headless merge, checks for conflicts, abandons the merge, and returns 0 (clean) or 1 (conflicts, with error messages printed).

```bash
# Pre-flight conflict check: verify a revision merges cleanly with trunk.
# Usage: preflight_conflict_check <revset> <trunk_bookmark>
# Returns 0 if clean, 1 if conflicts (with error messages printed).
preflight_conflict_check() {
    local revset="$1"
    local trunk_bookmark="$2"

    # Verify trunk bookmark exists
    if ! jj log -r "bookmarks(exact:$trunk_bookmark)" --no-graph -T'""' >/dev/null 2>&1; then
        preferr "trunk bookmark '$trunk_bookmark' not found"
        return 1
    fi

    # Create temporary merge commit in-repo only, no working copy
    local conflict_check_change_id
    conflict_check_change_id=$(jj new --no-edit -r "$trunk_bookmark" -r "$revset" 2>&1 | head -1 | cut -f4 -d' ')

    # Check for conflicts
    local has_conflicts
    has_conflicts=$(jj log -r "$conflict_check_change_id" --no-graph -T'if(conflict, "yes")')

    jj abandon "$conflict_check_change_id" >/dev/null 2>&1

    if [ -n "$has_conflicts" ]; then
        preferr "revision '$revset' conflicts with $trunk_bookmark"
        preferr "rebase onto $trunk_bookmark and resolve conflicts before pushing"
        return 1
    fi
    return 0
}
```

**Step 2: Update cmd_push to use the helper**

Replace lines 342-366 in `cmd_push` (from `# Pre-flight conflict check` through the `exit 1` after `has_conflicts`) with:

```bash
    # Pre-flight conflict check: verify revision could merge cleanly with trunk
    local trunk_bookmark
    trunk_bookmark=$(get_trunk_bookmark)
    preflight_conflict_check "$revset" "$trunk_bookmark" || exit 1
```

**Step 3: Run the e2e test to verify no regression**

Run: `./jjq-test`
Expected: All verifications pass (push still rejects conflicting revisions).

**Step 4: Commit**

```bash
jj desc -m "Extract preflight_conflict_check helper from cmd_push"
jj new
```

---

### Task 2: Add pre-flight conflict check to cmd_retry

Now that the helper exists, call it from `cmd_retry` after resolving the candidate revset but before allocating a new sequence ID.

**Files:**
- Modify: `jjq` (cmd_retry, after candidate_revset is resolved, ~line 755)

**Step 1: Write a test that retries a conflicting revision**

This is tested manually/via e2e. In the e2e test (`jjq-test`), the existing flow already retries with resolved revisions. We'll add a specific test later in Task 6 for the rejection case. For now, add the check and verify the existing e2e still passes.

**Step 2: Add the conflict check to cmd_retry**

Insert after the candidate revset resolution block (after line 755, just before the `# Push the candidate to the queue using next_id` comment) and after resolving `trunk_bookmark` for the trunk-moved warning:

```bash
    # Pre-flight conflict check: verify candidate merges cleanly with trunk
    preflight_conflict_check "$candidate_revset" "$trunk_bookmark" || exit 1
```

Note: `trunk_bookmark` is already resolved earlier in `cmd_retry` (line 714). The check goes right before `ensure_jjq` (line 758).

**Step 3: Run e2e test**

Run: `./jjq-test`
Expected: All verifications pass. The test's retry flow provides already-resolved revisions, so the conflict check should pass.

**Step 4: Commit**

```bash
jj desc -m "Add pre-flight conflict check to cmd_retry"
jj new
```

---

### Task 3: Define exit code constants

Define named constants for all exit codes at the top of the script, near the defaults section.

**Files:**
- Modify: `jjq:1-7` area (add constants after the defaults)

**Step 1: Add exit code constants**

After line 6 (`DEFAULT_MAX_FAILURES=3`), add:

```bash
# Exit codes
EXIT_SUCCESS=0
EXIT_CONFLICT=1
EXIT_CHECK_FAILED=2
EXIT_LOCK_HELD=3
EXIT_TRUNK_MOVED=4
EXIT_USAGE=10
# Internal sentinel (not returned to user)
_EXIT_QUEUE_EMPTY=99
```

**Step 2: Commit**

```bash
jj desc -m "Define exit code constants"
jj new
```

---

### Task 4: Update all exit/return sites to use exit code constants

Systematically update every `exit 1`, `return 1`, and `exit $result` in the script to use the appropriate constant.

**Files:**
- Modify: `jjq` (throughout)

**Step 1: Update usage() and argument errors**

- `usage()` line 212: `exit 1` → `exit $EXIT_USAGE`
- `cmd_run` unknown option (line 571): `exit 1` → `exit $EXIT_USAGE`

**Step 2: Update cmd_push exit sites**

- Line 339 (revset not found): `exit 1` → `exit $EXIT_USAGE`
- The `preflight_conflict_check` call: the helper returns 1 which is already `EXIT_CONFLICT`. But `cmd_push` does `|| exit 1`. Change to `|| exit $EXIT_CONFLICT`.

**Step 3: Update cmd_retry exit sites**

- Line 697 (usage): `exit 1` → `exit $EXIT_USAGE`
- Line 703 (validate_seq_id fails): already uses `exit 1` which should be `exit $EXIT_USAGE`
- Line 709 (failed item not found): `exit 1` → `exit $EXIT_USAGE`
- Line 735 (revset not found): `exit 1` → `exit $EXIT_USAGE`
- After preflight check: `exit 1` → `exit $EXIT_CONFLICT`

**Step 4: Update run_one_item return sites**

- Line 397 (queue empty): `return 2` → `return $_EXIT_QUEUE_EMPTY`
- Line 411 (check_command not configured): `return 1` → `return $EXIT_USAGE`
- Line 425 (lock held): `return 1` → `return $EXIT_LOCK_HELD`
- Line 463 (conflicts): `return 1` → `return $EXIT_CONFLICT`
- Line 495 (check failed): `return 1` → `return $EXIT_CHECK_FAILED`
- Line 511 (trunk moved): `return 1` → `return $EXIT_TRUNK_MOVED`

**Step 5: Update cmd_run to handle new codes**

Single mode: change `if [ $result -eq 2 ]` to `if [ $result -eq $_EXIT_QUEUE_EMPTY ]`.

Batch mode: change `$result -eq 2` to `$result -eq $_EXIT_QUEUE_EMPTY`. The `else` branch (failure) still does `exit 1` for now — Task 5 changes this.

**Step 6: Update cmd_delete exit sites**

- Line 807 (usage): `exit 1` → `exit $EXIT_USAGE`
- Line 814 (validate_seq_id): `exit 1` → `exit $EXIT_USAGE`
- Line 845 (not found): `exit 1` → `exit $EXIT_USAGE`

**Step 7: Update cmd_config exit sites**

- Line 878 (unknown key): `exit 1` → `exit $EXIT_USAGE`
- Line 898 (invalid value): `exit 1` → `exit $EXIT_USAGE`

**Step 8: Update validate_seq_id return sites**

- Lines 179, 185, 193: `return 1` → `return $EXIT_USAGE`

**Step 9: Update preflight_conflict_check return**

- The "trunk bookmark not found" case: `return 1` → `return $EXIT_USAGE`
- The "has conflicts" case: `return 1` → `return $EXIT_CONFLICT`

**Step 10: Run e2e test**

Run: `./jjq-test`
Expected: All verifications pass. Exit code 0 paths unchanged, non-zero paths now use specific codes but the test doesn't check exit codes (it checks output text).

**Step 11: Commit**

```bash
jj desc -m "Use distinct exit codes for conflict, check, lock, trunk-moved, and usage errors"
jj new
```

---

### Task 5: Update run --all to continue past failures

Now that exit codes distinguish lock-held from merge failures, update `cmd_run --all` to continue past conflict and check failures.

**Files:**
- Modify: `jjq` (cmd_run, lines 576-598)

**Step 1: Rewrite the batch loop**

Replace the `cmd_run` batch mode block (from `local merged_count=0` through the end of the while loop) with:

```bash
        local merged_count=0
        local failed_count=0
        local first_failure=""
        while true; do
            local result
            run_one_item && result=0 || result=$?
            if [ $result -eq $EXIT_SUCCESS ]; then
                merged_count=$((merged_count + 1))
            elif [ $result -eq $_EXIT_QUEUE_EMPTY ]; then
                break
            elif [ $result -eq $EXIT_LOCK_HELD ]; then
                # Can't process anything while another runner is active
                if [ $merged_count -gt 0 ] || [ $failed_count -gt 0 ]; then
                    prefout "processed $merged_count item(s), $failed_count failed (lock held, stopping)"
                fi
                exit $EXIT_LOCK_HELD
            else
                # Conflict or check failure - skip and continue
                failed_count=$((failed_count + 1))
                if [ -z "$first_failure" ]; then
                    first_failure=$result
                fi
            fi
        done

        local total=$((merged_count + failed_count))
        if [ $total -gt 0 ]; then
            if [ $failed_count -eq 0 ]; then
                prefout "processed $merged_count item(s)"
            else
                prefout "processed $merged_count item(s), $failed_count failed"
            fi
        fi

        if [ $failed_count -gt 0 ]; then
            exit "$first_failure"
        fi
        exit $EXIT_SUCCESS
```

**Step 2: Run e2e test**

Run: `./jjq-test`
Expected: All verifications pass. The e2e test processes items one at a time (not --all), so this doesn't change its behavior, but verifies nothing is broken.

**Step 3: Commit**

```bash
jj desc -m "run --all: continue past failures, bail on lock held"
jj new
```

---

### Task 6: Update SPEC.md

Document the three new behaviors in SPEC.md.

**Files:**
- Modify: `SPEC.md`

**Step 1: Add pre-flight conflict check section**

After the "Pushing to the queue" section (around line 56), add a paragraph:

```markdown
Before queuing, jjq performs a pre-flight conflict check by creating a
temporary headless merge commit between the candidate revision and the current
trunk. If the merge would produce conflicts, jjq rejects the push with
guidance to rebase and resolve conflicts first. The temporary merge commit is
abandoned immediately after the check.
```

**Step 2: Add the same to Retries section**

After the existing retry text (around line 213), add:

```markdown
Before re-queuing, jjq performs the same pre-flight conflict check as push. If
the candidate revision conflicts with the current trunk, the retry is rejected
with guidance to resolve conflicts first.
```

**Step 3: Add batch run behavior**

In the "Running the queue" section (around line 86), add:

```markdown
When running in batch mode (`--all`), jjq processes all queue items in
sequence. If an item fails (due to conflict or check failure), it is marked as
failed and processing continues with the next item. The only exception is a
lock-held condition, which halts batch processing immediately since no further
items can be processed while another runner is active. Batch mode exits with
code 0 if all items merged successfully, or the exit code of the first failure
if any items failed.
```

**Step 4: Add exit codes section**

Add a new section after "Conforming implementation behaviors" (end of file):

```markdown
### Exit codes

jjq uses distinct exit codes to allow programmatic use:

| Exit Code | Meaning           | When                                          |
|-----------|-------------------|-----------------------------------------------|
| 0         | Success           | Item merged, item queued, queue empty, etc.    |
| 1         | Merge conflict    | Pre-flight check (push/retry) or during run   |
| 2         | Check failed      | User's check command returned non-zero         |
| 3         | Lock held         | Another runner is active                       |
| 4         | Trunk moved       | Trunk bookmark advanced during run             |
| 10        | Usage/input error | Bad arguments, item not found, invalid revset  |
```

**Step 5: Commit**

```bash
jj desc -m "Update SPEC.md with conflict checks, batch run, and exit codes"
jj new
```

---

### Task 7: Add tests for new behavior

Update `jjq-test` to exercise the new behaviors: retry conflict rejection, run --all continuing past failures, and exit code values.

**Files:**
- Modify: `jjq-test`

**Step 1: Add a test for retry conflict rejection**

In `cmd_run`, after the queue processing loop, add a test that creates a conflicting revision and verifies `jjq retry` rejects it. Insert before the "Final status" section:

```bash
    echo ""
    echo "=== Testing retry conflict rejection ==="
    # Create a revision that conflicts with current main
    jj new main -m "conflicting change"
    # Make a change that will conflict with current main state
    echo "CONFLICT" > main.go
    conflict_rev=$(jj log -r @ --no-graph -T'change_id.short()')

    # Push should also reject this
    push_output=$(jjq push "$conflict_rev" 2>&1) && push_ok=true || push_ok=false
    if ! $push_ok && echo "$push_output" | grep -q "conflicts with"; then
        echo "  PASS: push correctly rejects conflicting revision"
    else
        echo "  FAIL: push should reject conflicting revision"
    fi

    # Clean up
    jj abandon
    jj new main
```

**Step 2: Add a test for run --all continuing past failures**

This is harder to test in the existing e2e flow because the test resolves conflicts inline. Consider adding a dedicated section. In `cmd_verify`, add a check that exercises `run --all`:

```bash
    # Test run --all with mixed results
    echo "Testing run --all batch behavior..."
    # Queue two items: one clean, one that will fail check
    jj new main -m "clean change"
    echo '# another line' >> README.md
    local clean_rev
    clean_rev=$(jj log -r @ --no-graph -T'change_id.short()')

    jjq push "$clean_rev"
    # Temporarily set check to fail
    jjq config check_command "sh -c 'exit 1'"

    local batch_output
    batch_output=$(jjq run --all 2>&1) && batch_ok=true || batch_ok=false

    # Restore check command
    jjq config check_command "make"

    if ! $batch_ok; then
        echo "  PASS: run --all exits non-zero when items fail"
    else
        echo "  FAIL: run --all should exit non-zero when items fail"
        ((errors++))
    fi
```

Note: This test is a starting point. The exact assertions depend on what items are available in the queue at verify time. The implementer should adapt this to the actual test flow — the key thing to verify is that `run --all` doesn't stop after the first failure and the summary line shows both merged and failed counts.

**Step 3: Add exit code value checks**

After the retry conflict rejection test, verify the exit code is specifically `EXIT_CONFLICT` (1), not just non-zero:

```bash
    push_exit=0
    jjq push "$conflict_rev" 2>/dev/null || push_exit=$?
    if [ $push_exit -eq 1 ]; then
        echo "  PASS: push conflict returns exit code 1"
    else
        echo "  FAIL: push conflict returned $push_exit, expected 1"
    fi
```

**Step 4: Run e2e test**

Run: `./jjq-test`
Expected: All verifications pass including new tests.

**Step 5: Commit**

```bash
jj desc -m "Add tests for retry conflict rejection, run --all resilience, exit codes"
jj new
```

---

### Task 8: Update design doc with implementation notes

Mark the design doc as implemented.

**Files:**
- Modify: `docs/plans/2026-01-28-conflict-checks-exit-codes-design.md`

**Step 1: Add status line at top**

After the date line, add:

```markdown
Status: Implemented
```

**Step 2: Commit**

```bash
jj desc -m "Mark conflict checks / exit codes design as implemented"
jj new
```
