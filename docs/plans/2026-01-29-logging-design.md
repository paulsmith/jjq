# jjq Logging Design

## Overview

Add comprehensive logging to jjq to provide visibility into merge queue operations, aid in debugging failures, and maintain an audit trail of queue activity.

## Goals

- Record all jjq operations (push, run, delete, config, clean) with sufficient detail for debugging
- Capture full check command output (stdout/stderr) for failed runs
- Provide human-readable log viewing via CLI
- Support programmatic access to logs for tooling/analysis
- Handle edge cases like process crashes, trunk movement, conflicts

## Non-Goals

- Log rotation/cleanup (future work)
- Real-time log streaming/tailing (future enhancement)
- Filtering by operation type, date range, etc. (future enhancement)

## Storage

### Format

JSONL (JSON Lines) - one JSON object per line.

**Rationale:**
- Append-only semantics match the use case perfectly
- Bash-friendly: easy to append with `echo` + `>>` redirection
- Tooling-friendly: `jq`, `grep`, `tail` all work naturally
- Forward-compatible: can evolve schema by adding fields without breaking old readers
- No parsing complexity compared to formats requiring full file rewrites

### Location

`log.jsonl` stored on the `jjq/_/_` branch, alongside other jjq metadata (config files, sequence_id).

**Benefits:**
- Co-located with other jjq state
- Versioned (can see historical log states via jj)
- Isolated from user working copy
- Survives operations that might delete workspaces

### Concurrency

**No additional locking required.** The existing lock system and jj's transaction model are sufficient:

- Operations that edit the metadata branch (push, config) either hold a lock or are atomic
- Operations that create new commits (delete, clean) leverage jj's optimistic concurrency
- If concurrent appends occur, jj's three-way merge will concatenate JSONL entries (line-based merging)

## Schema

### Common Fields

All log entries include:

| Field | Type | Description |
|-------|------|-------------|
| `log_format_version` | integer | Schema version (currently 1) |
| `timestamp` | string | ISO 8601 UTC timestamp (e.g., "2026-01-29T16:30:00Z") |
| `jjq_version` | string | jjq version (e.g., "1.0") |
| `hostname` | string | Output of `hostname` command |
| `operation` | string | Operation type: "push", "run", "delete", "config", "clean" |

### push

Records queuing a revision or rejecting it due to pre-flight conflict check.

**Success (queued):**
```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:30:00Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "push",
  "sequence_id": 42,
  "change_id": "abc123",
  "commit_id": "abc123def456...",
  "revset": "@",
  "trunk_commit_id": "main123...",
  "outcome": "queued"
}
```

**Failure (pre-flight conflict):**
```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:30:01Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "push",
  "change_id": "conflict1",
  "commit_id": "bad789...",
  "revset": "@",
  "trunk_commit_id": "main456...",
  "outcome": "rejected_conflict"
}
```

**Note:** No `sequence_id` when rejected (never obtained one).

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `sequence_id` | integer | Only if queued | Queue sequence ID |
| `change_id` | string | Yes | jj change ID (short form) |
| `commit_id` | string | Yes | Full commit ID of pushed revision |
| `revset` | string | Yes | User-provided revset argument |
| `trunk_commit_id` | string | Yes | Trunk commit ID at push time |
| `outcome` | string | Yes | "queued" or "rejected_conflict" |

### run

Records a merge attempt. Single entry written at completion.

**Success:**
```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:30:45Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "run",
  "sequence_id": 42,
  "change_id": "abc123",
  "candidate_commit_id": "abc123def...",
  "trunk_commit_id": "main123...",
  "merge_commit_id": "merged789...",
  "check_command": "make test",
  "outcome": "success",
  "duration_seconds": 12.4,
  "workspace_path": "/tmp/jjq-xyz"
}
```

**Conflict (merge has conflicts):**
```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:31:12Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "run",
  "sequence_id": 43,
  "change_id": "xyz789",
  "candidate_commit_id": "xyz789abc...",
  "trunk_commit_id": "main123...",
  "outcome": "conflict",
  "duration_seconds": 0.3,
  "workspace_path": "/tmp/jjq-abc"
}
```

**Check failed:**
```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:32:01Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "run",
  "sequence_id": 44,
  "change_id": "uvw321",
  "candidate_commit_id": "uvw321def...",
  "trunk_commit_id": "main123...",
  "check_command": "make test",
  "check_exit_code": 2,
  "check_stdout_base64": "dGVzdCBmYWlsZWQK",
  "check_stderr_base64": "",
  "outcome": "check_failed",
  "duration_seconds": 45.2,
  "workspace_path": "/tmp/jjq-uvw"
}
```

**Trunk moved (trunk bookmark changed during run):**
```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:33:00Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "run",
  "sequence_id": 45,
  "change_id": "moved1",
  "candidate_commit_id": "moved1abc...",
  "trunk_commit_id_start": "main123...",
  "trunk_commit_id_end": "main456...",
  "outcome": "trunk_moved",
  "duration_seconds": 30.1
}
```

**Note:** For trunk_moved, no `workspace_path` (workspace was cleaned up), no check output fields.

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `sequence_id` | integer | Yes | Queue sequence ID |
| `change_id` | string | Yes | jj change ID (short form) |
| `candidate_commit_id` | string | Yes | Full commit ID of candidate revision |
| `trunk_commit_id` | string | For success/conflict/check_failed | Trunk commit ID at run start |
| `trunk_commit_id_start` | string | For trunk_moved | Trunk at start |
| `trunk_commit_id_end` | string | For trunk_moved | Trunk at detection |
| `merge_commit_id` | string | For success only | Commit ID of successful merge |
| `check_command` | string | For success/check_failed | Command that was run |
| `outcome` | string | Yes | "success", "conflict", "check_failed", or "trunk_moved" |
| `duration_seconds` | float | Yes | Time from run start to completion |
| `workspace_path` | string | For success/conflict/check_failed | Path to workspace |
| `check_exit_code` | integer | For check_failed | Exit code from check command |
| `check_stdout_base64` | string | For check_failed | Base64-encoded stdout (empty string if no output) |
| `check_stderr_base64` | string | For check_failed | Base64-encoded stderr (empty string if no output) |

**Base64 encoding rationale:** JSON strings must be valid UTF-8. Check commands may output binary data or non-UTF-8 sequences. Base64 encoding ensures any bytes can be safely stored without data loss or JSON validity issues.

**Crash detection:** If a workspace exists but has no corresponding log entry, that indicates a process crash. No separate "run_start" entry is needed.

### delete

Records removal of a queue or failed item.

```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:34:00Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "delete",
  "sequence_id": 43,
  "item_type": "failed",
  "workspace_path": "/tmp/jjq-abc"
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `sequence_id` | integer | Yes | Sequence ID of deleted item |
| `item_type` | string | Yes | "queue" or "failed" |
| `workspace_path` | string | If applicable | Path to workspace that was removed |

### config

Records configuration changes.

```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:35:00Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "config",
  "key": "check_command",
  "value": "make test",
  "previous_value": ""
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `key` | string | Yes | Config key being set |
| `value` | string | Yes | New value |
| `previous_value` | string | Yes | Previous value (empty string if not set) |

### clean

Records workspace cleanup.

```json
{
  "log_format_version": 1,
  "timestamp": "2026-01-29T16:36:00Z",
  "jjq_version": "1.0",
  "hostname": "devbox",
  "operation": "clean",
  "workspaces": [
    {"sequence_id": 43, "workspace_path": "/tmp/jjq-abc"},
    {"sequence_id": 44, "workspace_path": "/tmp/jjq-uvw"}
  ]
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `workspaces` | array of objects | Yes | Array of cleaned workspaces |
| `workspaces[].sequence_id` | integer | Yes | Sequence ID |
| `workspaces[].workspace_path` | string | Yes | Path that was removed |

## CLI

### jjq log

Show recent log entries in human-readable format.

**Usage:**
```bash
jjq log          # Show last 20 entries (default)
jjq log N        # Show last N entries
jjq log --json   # Output raw JSONL
```

### Human-readable format

**Design principle:** Compact for success, detailed for failures.

**Success entries (one line):**
```
2026-01-29 16:30:00  push #42 (abc123) queued
2026-01-29 16:30:45  run #42 succeeded in 12.4s → main@def456
2026-01-29 16:35:00  config: check_command = "make test"
```

**Failure entries (multi-line with details):**
```
2026-01-29 16:31:12  run #43 conflict (workspace: /tmp/jjq-abc)
2026-01-29 16:32:01  run #44 check failed (exit 2, workspace: /tmp/jjq-uvw)
  check_command: make test
  stdout: [first 5 lines of decoded output]
  stderr: [first 5 lines of decoded output]
```

**Color support:** If stdout is a TTY, use colors:
- Green for success outcomes
- Red for failure outcomes
- Yellow for warnings/conflicts

### JSON output

```bash
jjq log --json N    # Output last N entries as JSONL
jjq log --json      # Output last 20 entries as JSONL
```

Output is raw JSONL (not wrapped in array) to allow streaming processing:
```bash
jjq log --json 100 | while IFS= read -r line; do
  echo "$line" | jq '.sequence_id'
done
```

## Implementation Notes

### Helper functions

```bash
# Get jjq version
get_jjq_version() {
  echo "1.0"  # Hardcoded for now, could read from VERSION file
}

# Append a log entry to log.jsonl
log_append() {
  local json="$1"

  # Ensure jjq metadata branch exists
  ensure_jjq

  # Create workspace for metadata branch
  local ws_path
  ws_path=$(mktemp -d)
  jj workspace add --name "jjq-log-$$" --revision "$jjq_bookmark" "$ws_path"

  # Append to log.jsonl
  echo "$json" >> "$ws_path/log.jsonl"

  # Edit commit (don't create empty commits)
  jj desc -r "$jjq_bookmark" -m "log: append entry"

  # Clean up workspace
  jj workspace forget "jjq-log-$$"
  rm -rf "$ws_path"
}
```

### Measuring duration

```bash
run_one_item() {
  local start_time
  start_time=$(date +%s)

  # ... do work ...

  local end_time
  end_time=$(date +%s)
  local duration=$((end_time - start_time))

  # Build log entry with duration
}
```

### Base64 encoding

```bash
# Encode stdout/stderr for log entry
local stdout_b64
stdout_b64=$(echo "$check_stdout" | base64)

local stderr_b64
stderr_b64=$(echo "$check_stderr" | base64)
```

### Building JSON

```bash
# Simple case (no special chars)
local json="{\"log_format_version\":1,\"timestamp\":\"$timestamp\",\"operation\":\"push\",...}"

# For complex strings (check output), use jq to build JSON safely
local json
json=$(jq -n \
  --arg timestamp "$timestamp" \
  --arg stdout_b64 "$stdout_b64" \
  '{log_format_version: 1, timestamp: $timestamp, check_stdout_base64: $stdout_b64}')
```

## Testing

Add to `jjq-test`:

1. Verify JSONL validity: each line parses as valid JSON
2. Verify required fields are present for each operation type
3. Verify base64 decoding works correctly
4. Test `jjq log` human-readable output
5. Test `jjq log --json` raw output
6. Verify log entries created for all operation types

## Future Enhancements

- Log rotation/cleanup strategy
- Filtering: `--operation=run`, `--sequence-id=42`, `--failed`
- Date range filtering: `--since`, `--until`
- Real-time log streaming: `jjq log --follow`
- Aggregates: success rate, average duration, etc.
- Export to other formats: CSV, SQLite

## Migration

No backwards compatibility needed. The current `log_op` function using commit messages will be replaced entirely with the JSONL-based system.

Existing installations will have empty `log.jsonl` initially. Historical information from old commit messages will not be migrated.
