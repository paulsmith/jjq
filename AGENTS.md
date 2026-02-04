# AGENTS.md - AI Assistant Context for jjq

## Project Overview

jjq is a merge queue CLI tool for jj (Jujutsu VCS).

## Key Files / Paths

- `src` - root of the Rust implementation
- `docs/README.md` - high-level overview
- `docs/specification.md` - RFC-style specification
- `jjq-test` - End-to-end test script

## Architecture

### Data Storage

jjq stores all state in the jj repository itself:

1. **Bookmarks** - Used for queue items and failed items
   - `jjq/queue/NNNNNN` - Queued items (6-digit zero-padded sequence ID)
   - `jjq/failed/NNNNNN` - Failed merge attempts

2. **Filesystem locks** - Uses `mkdir` atomicity in `.jj/jjq-locks/`
   - `run` - Ensures only one queue runner at a time
   - `id` - Protects sequence ID allocation

3. **Isolated branch** - `jjq/_/_` bookmark points to a branch parented to `root()` that holds:
   - `last_id` file - Current sequence ID
   - `config/` directory - User configuration
   - Commit messages serve as operation log (with trailers for structured data)

### Commands

| Command | Function |
|---------|----------|
| `init [--trunk <bookmark>] [--check <cmd>]` | Initialize jjq, set trunk and check command |
| `push <revset>` | Add revision to queue (idempotent: clears stale entries for same change ID) |
| `run [--all]` | Process next queue item, or all items in batch |
| `check [--rev <revset>]` | Run check command against a revision (default @) |
| `status [id] [--json] [--resolve]` | Show queue and recent failures; supports JSON output and single-item detail view |
| `delete <id>` | Remove item from queue/failed |
| `config [key] [value]` | Get/set configuration |
| `clean` | Remove failed workspaces |
| `doctor` | Validate config, locks, and workspace preconditions |

### Exit Codes

| Code | Constant | Meaning |
|------|----------|---------|
| 0 | `EXIT_SUCCESS` | Success |
| 1 | `EXIT_CONFLICT` | Merge conflict (pre-flight or during run) |
| 2 | `EXIT_CHECK_FAILED` | User's check command returned non-zero |
| 3 | `EXIT_LOCK_HELD` | Another runner is active |
| 4 | `EXIT_TRUNK_MOVED` | Trunk bookmark advanced during run |
| 10 | `EXIT_USAGE` | Bad arguments, item not found, invalid revset |

### Key Concepts

- **Sequence ID** - Monotonically increasing integer for FIFO ordering
- **Merge-to-be** - A commit with two parents: trunk and candidate revision
- **Runner workspace** - Temporary jj workspace in `/tmp` for running checks
- **Check command** - User-configured shell command that determines success/failure
- **Pre-flight conflict check** - Headless merge commit to verify clean merge before queuing
- **Idempotent push** - Re-pushing a change ID clears stale queue/failed entries; same commit ID is rejected as duplicate

### Concurrency

- `mkdir` atomicity is used as a mutex (mkdir fails if dir exists)
- Lock dirs stored in `.jj/jjq-locks/` (outside jj's tracked areas)
- `run` lock ensures only one queue runner at a time
- `id` lock protects sequence ID allocation

### Batch Mode (`run --all`)

When running in batch mode, jjq processes all queue items in sequence:
- Failed items (conflict or check failure) are skipped and processing continues
- Lock-held (`EXIT_LOCK_HELD`) causes immediate bail (no further progress possible)
- Summary reports both merged and failed counts
- Exits 0 if all merged, first failure code if any failed

## Testing

```sh
./jjq-test
```

The test script creates a temporary jj repository with 4 PR branches (some with known conflicts), processes the merge queue, resolves conflicts deterministically, and verifies the final state. Tests also cover exit codes, conflict rejection, and batch mode resilience.

## Development Notes

- jjq is implemented in Rust
- use the Nix flake here for a development shell `nix develop` and to build the artifacts `nix build` -> `./result/bin/jjq`
- All jj interaction is via the `jj` CLI (no API)
- Uses `jj log -T'...'` templates for structured output parsing
- `run_quiet` helper suppresses output on success, shows on failure
- `log_op` records operations as commits with trailer metadata
- `preflight_conflict_check` creates a headless merge commit to test for conflicts without a workspace

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
