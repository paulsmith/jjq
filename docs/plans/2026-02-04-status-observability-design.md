# Status & Observability Design

## Goal

Make jjq's queue state machine-readable for agent orchestration, while
improving the human debugging experience for failures.

Primary driver: **agent/automation support**.

## Changes

### 1. Richer Trailers on Failed Items

Failed item commit descriptions currently store one trailer:

```
Failed: merge <id> (conflicts|check)

jjq-candidate: <change_id>
```

Extend to:

```
Failed: merge <id> (conflicts|check)

jjq-candidate: <change_id>
jjq-candidate-commit: <commit_id>
jjq-trunk: <trunk_commit_id>
jjq-workspace: <workspace_path>
jjq-failure: conflicts|check
```

- `jjq-candidate-commit`: the commit ID of the original candidate at
  queue-processing time.
- `jjq-trunk`: the trunk commit ID at queue-processing time (what the
  candidate was merged against).
- `jjq-workspace`: filesystem path of the kept workspace for
  investigation.
- `jjq-failure`: machine-readable failure reason (`conflicts` or
  `check`).

**Queued items are left as bare bookmarks.** Their change ID, commit ID,
and description are resolved dynamically at query time. This avoids
modifying the user's commit description.

### 2. `jjq status --json`

Add a `--json` flag to the status command. Output:

```json
{
  "running": true,
  "queue": [
    {
      "id": 42,
      "change_id": "xopxuxzw",
      "commit_id": "abc123def456...",
      "description": "Add feature X"
    }
  ],
  "failed": [
    {
      "id": 41,
      "candidate_change_id": "yqrstuv",
      "candidate_commit_id": "def456abc789...",
      "description": "Fix bug Y",
      "trunk_commit_id": "ghi789jkl012...",
      "workspace_path": "/tmp/jjq-run-000041",
      "failure_reason": "check"
    }
  ]
}
```

Field semantics:

- **Queue items**: `change_id`/`commit_id` are the user's original
  revision (resolved dynamically from the bookmark target).
  `description` is the first line of the user's commit message.

- **Failed items**: `candidate_change_id`/`candidate_commit_id` are the
  original user revision (from trailers). `description` is the
  **original candidate's** commit message first line (resolved from the
  candidate change ID), not the jjq failure message. `failure_reason`
  and `trunk_commit_id` come from trailers. `workspace_path` is the
  kept workspace for investigation.

- `running`: whether the run lock is currently held.

- `max_failures` limit still applies to the `failed` array.

### 3. Single-Item Filters on `status`

Instead of a separate `show` command, extend `status` with filters:

```
jjq status [ID] [--json] [--resolve <change_id>]
```

- `jjq status 42` — single-item text view by sequence ID.
- `jjq status 42 --json` — single-item JSON view.
- `jjq status --resolve <change_id>` — look up by candidate change ID.
- `jjq status --resolve <change_id> --json` — same, JSON output.

`ID` and `--resolve` are mutually exclusive.

Single-item lookup searches both queue and failed bookmarks. Items
beyond the `max_failures` window are accessible by explicit ID or
`--resolve`.

**Text output for a queued item:**

```
Queue item 42
  Change ID:   xopxuxzw
  Commit ID:   abc123def456...
  Description: Add feature X
```

**Text output for a failed item:**

```
Failed item 41
  Candidate:   yqrstuv (def456abc789...)
  Description: Fix bug Y
  Failure:     check
  Trunk:       ghi789jkl012...
  Workspace:   /tmp/jjq-run-000041

To resolve:
  1. Fix the issue and create a new revision
  2. Run: jjq push <fixed-revset>
```

**JSON output** for single-item modes returns the same object shape as
the corresponding entry in the `status --json` arrays, but as a
top-level object (not wrapped in an array).

## Data Storage

All metadata stays in commit description trailers (no new metadata
branch files). This keeps data co-located with the bookmark target and
queryable with jj templates.

Trailers use the `jjq-` prefix namespace, one per line, in the commit
description body (after a blank line separator from the subject).

## Implementation Notes

- The `run_one()` function needs to write the new trailers at the two
  failure points (conflicts and check failure). The trunk commit ID and
  workspace path are already available in local variables at those
  points.

- For `--json` output, use `serde_json` (already a Rust ecosystem
  standard). Define serializable structs for the output shapes.

- For `--resolve`, scan failed item descriptions for matching
  `jjq-candidate` trailer, and scan queue bookmark targets for matching
  change ID.

- The `resolve_revset_full()` function in `jj.rs` already returns both
  change ID and commit ID, which is needed for queue item JSON output.

- A trailer-parsing helper should be extracted (generalizing the
  existing `extract_candidate_trailer`) to parse all `jjq-*` trailers
  from a description into a map.
