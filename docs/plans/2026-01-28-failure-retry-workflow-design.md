# Failure & Retry Workflow Improvements

Design for addressing issues identified in the 2026-01-28 stress test.

## Summary

Five improvements to the failure/retry experience:

1. **Retry lineage tracking** - Track which failed item a retry came from
2. **Conflict resolution guidance** - Actionable next steps on failure
3. **Trunk movement warning** - Alert when retrying against moved trunk
4. **Smart status annotations** - Show retry relationships when relevant
5. **Workspace cleanup** - Clean up on delete + bulk clean command

## 1. Retry Lineage Tracking

### Problem

When item 2 fails and is retried, it becomes item 5. The connection is lost.

### Solution

Track retry relationships via commit message trailers:

```
jjq-retry-of: 2
```

**Implementation:**

1. `jjq retry <id>` writes a file `pending-retries/<new-id>` on the metadata branch containing the original ID
2. `jjq run` reads this file when processing the item
3. When creating the merge commit, append the `jjq-retry-of: <original>` trailer
4. Delete the `pending-retries/<new-id>` file after the merge commit is created

This keeps bookmarks clean and makes lineage visible in normal `jj log`.

### Spec Changes

- Add `pending-retries/` directory to metadata branch structure
- Update `retry` command to write pending-retry metadata
- Update `run` command to read pending-retry metadata and add trailer

## 2. Conflict Resolution Guidance

### Problem

Failure output doesn't tell users what to do next.

### Solution

Add actionable guidance to failure output:

**For conflicts:**
```
jjq: merge 2 has conflicts, marked as failed
jjq: workspace: /var/folders/.../tmp.abc123
jjq:
jjq: To resolve:
jjq:   1. Fix conflicts in the workspace above (or create a new revision)
jjq:   2. Run: jjq retry 2 <fixed-revset>
```

**For check failures:**
```
jjq: merge 2 failed check, marked as failed
jjq: workspace: /var/folders/.../tmp.abc123
jjq:
jjq: To resolve:
jjq:   1. Fix the issue and create a new revision
jjq:   2. Run: jjq retry 2 <fixed-revset>
```

### Spec Changes

- Update `run` command output format on failure

## 3. Trunk Movement Warning on Retry

### Problem

Users retry a fix based on old trunk, but trunk has moved. The fix conflicts again.

### Solution

When `jjq retry` is invoked, compare current trunk with the trunk used in the failed merge (parent 1). If different, warn:

```
jjq: warning: trunk has advanced since this failure
jjq: hint: consider rebasing your fix onto current trunk first
jjq: queued 2 → 5 for merge
```

The warning is informational only - retry still proceeds. The pre-flight conflict check catches actual conflicts.

**Detection:** Compare commit ID of failed merge's parent 1 with current trunk bookmark's commit ID.

### Spec Changes

- Update `retry` command to check trunk movement and emit warning

## 4. Smart Status Annotations

### Problem

`jjq status` shows pending and failed items separately with no visible relationship.

### Solution

Show retry relationships inline when retries exist:

```
Pending:
  1: kzwykqkr Add user authentication
  5: xyzabcde Fix login validation (retry of 2)

Failed:
  2: lmnoprst Fix login validation → retrying as 5
  3: qrstuvwx Refactor database layer
```

- Failed items show `→ retrying as N` when a retry is pending
- Pending retries show `(retry of N)`
- No annotations when no retries exist (keeps output clean)

**Implementation:** Cross-reference pending items' retry metadata with failed items. Build map of `original_id → retry_id`, annotate both directions.

### Spec Changes

- Update `status` command output format

## 5. Workspace Cleanup

### Problem

Failed merges leave temp workspaces that accumulate.

### Solution

Two mechanisms:

**A. `jjq delete <id>` cleans associated workspace:**

```
jjq: deleted 3
jjq: removed workspace /var/folders/.../tmp.abc123
```

Find workspace by name pattern `jjq-run-<padded-id>`. If registered, forget and remove directory.

**B. New `jjq clean` command for bulk cleanup:**

```
jjq clean
```

Output:
```
jjq: removed 3 workspaces
jjq:   /var/folders/.../tmp.abc123 (failed item 2)
jjq:   /var/folders/.../tmp.def456 (failed item 5)
jjq:   /var/folders/.../tmp.ghi789 (orphaned)
```

Removes all jjq workspaces - both those with corresponding failed items and orphaned ones. Acts immediately without confirmation.

**Implementation:** Enumerate workspaces matching `jjq-run-*` via `jj workspace list`. For each, forget and remove. Label as "failed item N" or "orphaned" based on whether `jjq/failed/<id>` exists.

### Spec Changes

- Update `delete` command to clean up workspace
- Add new `clean` command

## Implementation Order

Suggested order based on dependencies and impact:

1. **Conflict resolution guidance** - Standalone change to `run` output
2. **Workspace cleanup** - Standalone, addresses immediate pain point
3. **Retry lineage tracking** - Foundation for status annotations
4. **Trunk movement warning** - Builds on lineage tracking (needs to read failed merge parents)
5. **Smart status annotations** - Requires lineage tracking to be in place
