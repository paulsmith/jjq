# AGENTS.md - AI Assistant Context for jjq

## Project Overview

jjq is a merge queue CLI tool for jj (Jujutsu VCS). It's implemented as a single bash script.

## Key Files

- `jjq` - The entire implementation (bash script)
- `SPEC.md` - Design specification
- `jjq-test` - End-to-end test script

## Architecture

### Data Storage

jjq stores all state in the jj repository itself:

1. **Bookmarks** - Used for queue items, failed items, and locks
   - `jjq/queue/NNNNNN` - Queued items (6-digit zero-padded sequence ID)
   - `jjq/failed/NNNNNN` - Failed merge attempts
   - `jjq/lock/run` - Global lock for queue runner
   - `jjq/lock/id` - Lock for sequence ID allocation

2. **Isolated branch** - `jjq/_/_` bookmark points to a branch parented to `root()` that holds:
   - `last_id` file - Current sequence ID
   - `config/` directory - User configuration
   - Commit messages serve as operation log (with trailers for structured data)

### Commands

| Command | Function |
|---------|----------|
| `push <revset>` | Add revision to queue |
| `run` | Process next queue item |
| `status` | Show queue and recent failures |
| `retry <id> [revset]` | Re-queue a failed item |
| `delete <id>` | Remove item from queue/failed |
| `config [key] [value]` | Get/set configuration |
| `clean [id\|all]` | Remove failed workspaces |
| `log [n]` | Show operation history |

### Key Concepts

- **Sequence ID** - Monotonically increasing integer for FIFO ordering
- **Merge-to-be** - A commit with two parents: trunk and candidate revision
- **Runner workspace** - Temporary jj workspace in `/tmp` for running checks
- **Check command** - User-configured shell command that determines success/failure

### Concurrency

- Bookmark creation is used as a mutex (create fails if exists)
- `jjq/lock/run` ensures only one queue runner at a time
- `jjq/lock/id` protects sequence ID allocation

## Testing

The project uses end-to-end testing via `test_e2e.sh`:

```sh
./test_e2e.sh
```

Tests create temporary jj repositories and exercise the full command flow.

## Development Notes

- All jj interaction is via the `jj` CLI (no API)
- Uses `jj log -T'...'` templates for structured output parsing
- `run_quiet` helper suppresses output on success, shows on failure
- `log_op` records operations as commits with trailer metadata

## Common Patterns

### Reading jjq state
```bash
jj bookmark list -r 'bookmarks(glob:"jjq/queue/??????")' -T'name ++"\n"'
```

### Creating workspace for operations
```bash
d=$(mktemp -d)
jj workspace add -r "$jjq_bookmark" --name "workspace-name" "$d"
# ... do work in $d ...
jj workspace forget "workspace-name"
rm -rf "$d"
```

### Logging operations
```bash
log_op "" "operation: summary" \
    "Operation: name" \
    "Key: value" \
    "Timestamp: $(timestamp)"
```
