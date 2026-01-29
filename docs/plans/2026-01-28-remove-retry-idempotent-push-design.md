# Remove retry command, make push idempotent

Date: 2026-01-28
Status: Proposed

## Context

Stress testing revealed that cascading conflicts are the biggest UX pain point
in jjq. When a queued revision conflicts with trunk, the user resolves
conflicts and retries — but trunk can move again before the retry is processed,
causing another conflict. In one test, a single change required 3 resolution
cycles before merging.

The `retry` command also introduced significant complexity: retry lineage
tracking via `pending-retries/` metadata, status annotations showing retry
relationships, trunk-movement warnings, and cleanup logic in `run_one_item`'s
success path. A related bug caused duplicate entries in `jjq status` when
multiple failed bookmarks referenced the same change ID.

Research into established merge queues (bors-ng, GitHub merge queue, Zuul)
shows that none of them have a retry command. The author fixes their change and
re-submits it. The queue picks up the updated version.

## Design

Remove `cmd_retry` entirely. Make `cmd_push` idempotent: pushing a change
that's already queued or failed replaces the old entry.

### Push behavior

After resolving the change ID and commit ID from the user's revset:

1. **Exact duplicate guard.** Scan `jjq/queue/*` bookmarks. If any points to
   the same commit ID, refuse with an error:

       jjq: revision already queued at 3

2. **Stale queue entry replacement.** If any queue bookmark points to the same
   change ID but a different commit ID, delete it:

       jjq: replacing queued entry 3

3. **Failed entry cleanup.** Scan `jjq/failed/*` bookmarks. If any points to
   the same change ID, delete it:

       jjq: clearing failed entry 2

4. **Preflight conflict check** against trunk. Unchanged from current behavior.

5. **Queue the revision** with a new sequence ID. Unchanged.

Steps 2 and 3 can both apply in a single push (e.g., the change has a stale
queue entry and a failed entry from an earlier attempt).

### User workflow for handling failures

```
$ jjq push mychange        # queued at 3
$ jjq run                  # fails — conflicts with trunk
$ jj rebase -r mychange -d main
$ # resolve conflicts
$ jjq push mychange        # clears failed 3, queued at 4
$ jjq run                  # success
```

The user never needs to learn a separate retry command. The mental model is:
"push puts the latest version of my change in the queue."

### What gets removed

- `cmd_retry` (~110 lines)
- `pending-retries/` metadata directory and all reads/writes
- Retry lineage tracking in `cmd_status` ("retry of X" / "retrying as Y")
- `jjq-retry-of:` trailers in merge commit descriptions
- Retry metadata cleanup in `run_one_item`'s success path
- The `retry` entry in the command dispatch case statement
- Retry-related tests in `jjq-test`

### What stays unchanged

- `run_one_item` — still creates a two-parent merge commit, tests, handles
  conflicts and failures the same way
- `cmd_delete` — still works for manually removing queue or failed entries
- `cmd_status` — simpler, shows queued and failed items without relationship
  tracking
- `cmd_clean` — still cleans up leftover workspaces
- All exit codes — unchanged
- Failure output from `run` — still prints resolution guidance pointing the
  user at `jjq push` instead of `jjq retry`

### Side effects

- Fixes the duplicate-entries-in-status bug: push cleans up old failed entries
  for a change ID before queuing, so multiple failed bookmarks for the same
  logical change cannot accumulate.
- Eliminates the cascading conflict race: there is no gap between "retry" and
  "run" where trunk can move. The user rebases onto current trunk, pushes, and
  the preflight check validates against trunk at that moment.
- Reduces the metadata branch churn: no more pending-retries writes/deletes.

## Scope

This is a breaking change to the CLI surface: `jjq retry` will no longer
exist. The specification, README, AGENTS.md, and test script need updating.
