# Pre-flight conflict checks, run --all resilience, and exit codes

Date: 2026-01-28
Status: Implemented

## Context

Stress testing with parallel agents revealed three UX gaps:

1. `retry` doesn't check for conflicts before queuing, so users only discover
   conflicts later during `run`.
2. `run --all` stops on the first failure, leaving remaining queue items
   unprocessed.
3. All failure modes return exit code 1, making programmatic use difficult.

## Changes

### 1. Pre-flight conflict check on retry

`cmd_retry` gets the same headless merge check that `cmd_push` already has
(added in commit 64dca08). After resolving the candidate revset but before
allocating a new sequence ID, retry creates a temporary merge commit with
`jj new --no-edit -r trunk -r candidate`, checks for conflicts via
`jj log -r <rev> -T'if(conflict, "yes")'`, and abandons it.

If conflicts exist, retry bails with guidance:

    jjq: revision '<revset>' conflicts with <trunk>
    jjq: rebase onto <trunk> and resolve conflicts before retrying

The existing trunk-moved warning stays — it's useful context. But it's now
backed by a hard check: even if the user ignores the warning, the conflict
check catches it.

### 2. `run --all` continues past failures

When `run_one_item` returns a failure code (conflict or check failure),
`cmd_run --all` increments a `failed_count` and continues processing the next
queue item instead of exiting.

After the loop completes (queue empty), output summarizes both counts:

    jjq: processed 3 item(s), 1 failed

Exit code: 0 if all items merged, 1 if any failed. Per-item output printed
during the loop already identifies which items failed and why.

**Lock held is special.** If `run_one_item` returns the lock-held exit code
(see section 3), `run --all` bails immediately since no further items can be
processed while another runner is active.

Single-item mode (`jjq run` without `--all`) is unchanged — it processes one
item and returns its exit code directly.

### 3. Distinct exit codes

Current state: everything returns 1. New scheme:

| Exit Code | Meaning           | When                                          |
|-----------|-------------------|-----------------------------------------------|
| 0         | Success           | Item merged, item queued, queue empty, etc.    |
| 1         | Merge conflict    | Pre-flight check (push/retry) or during run   |
| 2         | Check failed      | User's check command returned non-zero         |
| 3         | Lock held         | Another runner is active                       |
| 4         | Trunk moved       | Trunk bookmark advanced during run             |
| 10        | Usage/input error | Bad arguments, item not found, invalid revset  |

Applied across commands:

- `cmd_push`: exits 1 for conflict, 10 for bad revset or missing trunk.
- `cmd_retry`: exits 1 for conflict, 10 for item not found or bad revset.
- `run_one_item`: returns 1 for conflict, 2 for check failure, 3 for lock
  held, 4 for trunk moved. The internal "queue empty" sentinel uses a high
  value (99) since 2 is now taken.
- `cmd_run` (single mode): passes through the exit code from `run_one_item`.
- `cmd_run --all`: exits 0 for full success, uses the first failure code
  encountered if any items failed. Bails immediately on code 3 (lock held).

## Implementation notes

Implemented across 4 commits:

1. Extracted `preflight_conflict_check` helper from `cmd_push`, called from
   both `cmd_push` and `cmd_retry`.
2. Defined exit code constants (`EXIT_CONFLICT`, `EXIT_CHECK_FAILED`,
   `EXIT_LOCK_HELD`, `EXIT_TRUNK_MOVED`, `EXIT_USAGE`, `_EXIT_QUEUE_EMPTY`)
   and updated all exit/return sites.
3. Rewrote `cmd_run --all` to continue past conflict/check failures, bail
   immediately on lock held, and report summary with merged/failed counts.
