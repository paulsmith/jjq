# jjq Parallel Agent Testing Report

**Date:** 2026-01-27
**Tester:** Claude (orchestrating 3 subagents)
**Version:** Current development (bash implementation)

## Test Scenario

Created a Go web application with 3 features developed in parallel by independent subagents:

1. **Health endpoint** (`GET /health`) - returns `{"status":"ok"}`
2. **Echo endpoint** (`POST /echo`) - echoes request body
3. **Time endpoint** (`GET /time`) - returns current server time in JSON

Each subagent was instructed to:
- Create their own jj workspace from `main`
- Implement their feature with tests
- Run tests locally to verify
- Push to the jjq queue when done

The orchestrator then ran the queue to observe merge behavior.

### Configuration

```bash
jjq config check_command "go test ./..."
jjq config trunk_bookmark main
```

## Results Timeline

| Seq ID | Feature | Outcome | Notes |
|--------|---------|---------|-------|
| 1 | (conflicted) | Deleted | Race condition - two agents got same ID |
| 2 | Health | Failed (check) | golangci-lint version mismatch |
| 3 | Time | **Merged** | First successful merge |
| 4 | Echo | Failed (conflicts) | Conflicted with merged time endpoint |
| 5 | Health | Failed (conflicts) | Same conflict pattern |
| 6 | Echo (rebased) | **Merged** | Manually rebased onto trunk |
| 7 | Health (rebased) | Failed (conflicts) | Trunk moved again after echo merged |
| 8 | Health (rebased again) | **Merged** | Finally clean |

### Final Trunk State

```
○  81c699c7 Success: merge 000008 (health)
○  7ee37a8e Success: merge 000006 (echo)
○  79af5d1a Success: merge 000003 (time)
○  d4249f2a Initial commit: base web app skeleton
```

All 4 tests pass on final main.

---

## What Worked Well

### 1. Core Concept is Sound

jjq caught real conflicts that would have "worked on my branch" but failed when merged. Each feature passed tests in isolation but conflicted when combined - exactly the problem jjq is designed to solve.

### 2. FIFO Ordering Enforced Correctly

Items processed in strict sequence ID order, ensuring each merge validates against the true current trunk state. No shortcuts or out-of-order processing.

### 3. Status Command is Clear

```
jjq: Queued:
  3: xnuppvvvrvlw Add /time endpoint
  4: kltmkxlzxpsu Add /echo endpoint

jjq: Failed (recent):
  2: ypyzqwzkxyqu Failed: merge 000002 (check)
```

Easy to understand queue state at a glance. Shows both pending work and recent failures.

### 4. Failed Workspace Preservation

When merges fail, keeping the workspace around for debugging is genuinely useful:

```
jjq: merge 000004 has conflicts, marked as failed
jjq: workspace remains: /var/folders/.../tmp.l8INphRwlw
```

Could inspect conflict markers directly in the preserved workspace.

### 5. Retry Mechanism Works

`jjq retry N` correctly extracts the original candidate revision and re-queues it with a new sequence ID.

### 6. Configuration is Minimal and Sensible

Just two required settings (`trunk_bookmark`, `check_command`), stored in the repo itself via the metadata branch. No external config files to manage.

### 7. Delete Command is Useful

Easy to clean up failed items after fixing them with a new approach: `jjq delete 4`

---

## Issues and Improvement Opportunities

### 1. Race Condition on Push (Critical Bug)

**Observed:** Two subagents pushed simultaneously and both got sequence ID 1, creating a conflicted bookmark:

```
jjq/queue/000001 (conflicted):
  + ollsxppo Add /health endpoint
  + kltmkxlz Add /echo endpoint
```

**Expected:** The ID lock (`jjq/lock/id`) should serialize ID allocation.

**Impact:** In multi-developer scenarios, this could cause confusion and require manual cleanup.

**Suggested Fix:** Investigate why the lock didn't prevent concurrent ID allocation. Possibly the lock acquisition/release has a race window, or the lock isn't being held long enough during the read-modify-write of `last_id`.

---

### 2. Sparse Success Output

**Observed:** On successful merge, output is minimal:
```
jjq: processing queue item 000003
```

**Expected:** Clear success indication like:
```
jjq: processing queue item 000003
jjq: merged 000003 to trunk (now at abc1234)
```

**Impact:** Users have to run `jjq status` or `jj log` to confirm success.

**Suggested Fix:** Add explicit success message with the new trunk commit.

---

### 3. No "Rebase and Retry" Workflow

**Observed:** When a queued item conflicts, the workflow is tedious:
1. Understand the conflict (inspect preserved workspace)
2. Create a new workspace from current trunk
3. Manually re-apply the changes
4. Push as a new queue item
5. Delete the old failed item

**Suggested Feature:** `jjq rebase N` command that:
1. Finds the candidate revision from failed item N
2. Creates a workspace with that revision rebased onto current trunk
3. If no conflicts: automatically re-queues and deletes old failed item
4. If conflicts: leaves workspace for manual resolution, prints path

This is the most common recovery workflow and automating it would significantly improve UX.

---

### 4. No Continuous/Watch Mode

**Observed:** Had to manually run `jjq run` repeatedly after each completion.

**Suggested Features:**
- `jjq run --all` - Process entire queue, stopping on first failure
- `jjq run --watch` - Continuously watch for new queue items and process them
- `jjq run --loop` - Keep running until queue is empty (useful for CI)

---

### 5. Status Display Bug with Conflicted Bookmarks

**Observed:** When race condition created conflicted bookmarks, status output was garbled:
```
jjq: Queued:
  1: ollsxppovxvs Add /health endpointkltmkxlzxpsu Add /echo endpoint
```

Two entries concatenated on one line.

**Expected:** Either show both targets clearly, or warn about the conflict:
```
jjq: Queued:
  1: (CONFLICTED - multiple targets)
```

**Suggested Fix:** Detect conflicted bookmarks in status and handle gracefully.

---

### 6. Limited Visibility into Check Command Failures

**Observed:** When check command fails, only the exit message is shown. For long test output, you'd need to manually navigate to the preserved workspace.

**Suggested Feature:** `jjq log N` or `jjq show N` to display the check command output for a failed item. Could store stdout/stderr in the workspace or in commit metadata.

---

### 7. Workspace Accumulation

**Observed:** After testing, accumulated 12 workspaces including 4 `jjq-run-*` workspaces from failed merges.

**Current:** `jjq clean all` deletes temp directories but doesn't forget the jj workspaces.

**Suggested Enhancement:** `jjq clean --forget` option that also runs `jj workspace forget` for cleaned workspaces.

---

## Feature Requests Summary

| Priority | Feature | Description |
|----------|---------|-------------|
| Critical | Fix ID lock race | Prevent concurrent pushes from getting same ID |
| High | Success messages | Clear output when merge succeeds |
| High | `run --all` | Process entire queue in one command |
| Medium | `rebase N` | Automate the conflict-fix-retry workflow |
| Medium | Better conflict status | Handle conflicted bookmarks in status display |
| Low | `run --watch` | Daemon mode for continuous processing |
| Low | Check output logging | Store/display check command output for failures |
| Low | `clean --forget` | Also forget jj workspaces when cleaning |

---

## Conclusion

**jjq's core value proposition works.** It successfully prevented "it worked on my branch" problems by testing each merge against actual trunk state. The FIFO queue ensures deterministic merge ordering.

The main friction points are:
1. The ID lock race condition (bug)
2. Manual workflow for fixing conflicts and retrying
3. Need to run `jjq run` repeatedly

Fixing the race condition and adding `run --all` would make jjq significantly more usable for real parallel development workflows.
