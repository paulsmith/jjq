# AGENTS.md - AI Assistant Context for jjq

## Project Overview

jjq is a merge queue CLI tool for jj (Jujutsu VCS).

## Important things to remember

### jj

`jj squash` is interactive - it'll block your tool use waiting for input if
you are not careful

### Landing commits on main

The `main` bookmark is protected and immutable. Do NOT try to move it
directly with `jj bookmark set/move` or `jj tug`. Instead, use jjq itself:

```sh
jjq push @    # queue the commit
jjq run       # land it on main
```

## Key Files / Paths

- `src/` - root of the Rust implementation
- `tests/e2e.rs` - end-to-end integration tests (insta snapshots)
- `docs/README.md` - high-level overview and design specification
- `docs/jjq.1` - man page (roff source); `docs/jjq.1.txt` is generated from it via `mandoc -Tascii -O width=78 docs/jjq.1 > docs/jjq.1.txt`

## Architecture

### Data Storage

jjq stores all state in the jj repository itself:

1. **Bookmarks** - Used for queue items, in-flight claims, and failed items
   - `jjq/queue/NNNNNN` - Queued items (6-digit zero-padded sequence ID)
   - `jjq/running/NNNNNN` - Item claimed by the active queue runner; with the run lock free it is a stale claim from a crashed runner (the next run requeues it)
   - `jjq/failed/NNNNNN` - Failed merge attempts

2. **Filesystem locks** - Uses `flock(2)` files in `.jj/jjq/locks/`
   - `run` - Ensures only one queue runner at a time; also held by `clean` for its whole cleanup
   - `meta` - Serializes all writes to the `jjq/_/_` metadata branch (sequence ID allocation, config, workspace records); reads take no lock
   - `check` - Held shared by each `jjq check` for its workspace's lifetime; `clean` probes it exclusively to refuse while a check is active
   - `mutation` - Blocking lock for full `push`, `requeue`, and `delete` mutations; serializes queue-state changes across those commands
   - `publication` - Couples producer queue-bookmark publication to its queued history event and runner claims to their claimed history event

3. **Filesystem layout** - `.jj/jjq/` is the home for jjq filesystem state under the shared repository `.jj` directory
   - `locks/` - `flock(2)` lock files shared by all workspaces for the repo
   - `run.log` - Combined stdout/stderr of the last check command, viewed by `jjq tail`
   - New filesystem state should live under `.jj/jjq/`, not as a new sibling path in `.jj/`

4. **Isolated branch** - `jjq/_/_` bookmark points to a branch parented to `root()` that holds:
   - `last_id` file - Current sequence ID
   - `config/` directory - User configuration
   - Commit messages serve as operation log (with trailers for structured data)

### Commands

| Command | Function |
|---------|----------|
| `init [--trunk <bookmark>] [--check <cmd>] [--strategy <strategy>]` | Initialize jjq, set trunk, check command, and strategy |
| `push <revset> [--allow-stack]` | Add revision to queue (idempotent: clears stale entries for same change ID; pushing the current workspace's @ moves the working copy to a new empty child); by default refuses multi-revision candidates unless `--allow-stack` is given |
| `run [--all] [--stop-on-failure] [--jobs N]` | Process next queue item, or all items in batch; merge batches may check up to N predicted states concurrently and still land FIFO |
| `check [--rev <revset>] [-v]` | Run check command against a revision (default @); -v shows workspace/env details |
| `status [id] [--json] [--resolve]` | Show running/queued items, recent failures (with conflicting file paths), and recently landed items; JSON output has `runner_active` (bool) plus `running`/`queue`/`failed`/`landed` arrays |
| `requeue <id>` | Re-push a failed item back onto the queue (with pre-flight conflict check); refuses an item being processed; recovers a stale claim back to the queue |
| `reject <id> --reason <text>` | Mark a queued or failed item as failed without running it, recording the reason; a failed item with a reason is terminal |
| `wait <revset> [--interval] [--timeout]` | Block until the candidate lands (exit 0) or is rejected (exit 1, reason on stdout); unknown/timeout exits 2 |
| `delete <id>` | Remove item from queue/failed; refuses an item being processed by an active runner (removes stale claims when the run lock is free) |
| `cancel <id>` | Immediately remove queued work or cooperatively stop a running check; idempotent after cancellation |
| `history [<id> | --change <change-id>] [--json]` | Show durable submission lifecycle events, including cancellation intent/outcome |
| `config [key] [value]` | Get/set configuration |
| `clean` | Remove all jjq-generated workspaces; refuses while a run or check is active; also runs the residue-cleanup pass |
| `doctor [--fix]` | Validate config, conflicted jjq bookmarks, sequence counter, locks, stale claims, and workspace preconditions; `--fix` repairs a conflicted metadata bookmark (keeps the side with the highest `last_id`) |
| `tail [--all] [--no-follow]` | View the last check output (with optional follow) |
| `quickstart` | Print a short guide for LLM agents |

### Exit Codes

| Code | Constant | Meaning |
|------|----------|---------|
| 0 | *(success)* | Success |
| 1 | `CONFLICT` | Merge conflict, check failed, check command not configured, trunk moved during run, or another queue runner is active |
| 2 | `PARTIAL` | Batch run processed some items but left failures |
| 3 | `LOCK_HELD` | Metadata lock still held after the wait timeout (a concurrent writer is stuck) |
| 10 | `USAGE` | Bad arguments, invalid revset, duplicate push of an already-queued commit, or an item ID not found in queue or failed (`status`/`delete`/`requeue`) |

### Config Keys

| Key | Default | Meaning |
|-----|---------|---------|
| `trunk_bookmark` | `main` | Bookmark pointing to trunk |
| `check_command` | *(set during init)* | Command run on merge candidates |
| `strategy` | `rebase` (new repos) | `rebase` or `merge` landing strategy |
| `require_single_revision` | `true` | Refuse candidates that are more than one revision atop trunk at push/requeue/run; `--allow-stack` exempts a single item |

### Key Concepts

- **Sequence ID** - Monotonically increasing integer for FIFO ordering
- **Runner workspace** - Temporary jj workspace in `/tmp` for running checks
- **Check command** - User-configured shell command that determines success/failure
- **Pre-flight conflict check** - Headless merge commit to verify clean merge before queuing
- **Idempotent push** - Re-pushing a change ID clears stale queue/failed entries; same commit ID is rejected as duplicate; a revision claimed by an active runner is refused ("currently being processed")
- **Running claim** - The runner renames `jjq/queue/N` to `jjq/running/N` under the run lock before processing; on success/failure the claim is deleted or becomes `jjq/failed/N`. Immediately before landing, the runner verifies the claim still exists and otherwise discards the result. Claims orphaned by a crash are moved back to the queue at the start of the next run ("recovered interrupted item N")
- **Requeue** - Re-pushes a failed item back onto the queue, running a pre-flight conflict check first; a shortcut for fixing and re-pushing manually
- **Landing strategy** - How the candidate is landed on trunk: `rebase` (default) or `merge`
- **Working-copy move-off** - `push`/`requeue` of the invoking workspace's working-copy commit parks the working copy on a new empty child (like `jj commit`), so later snapshots don't amend the queued commit — once landed, trunk would follow those rewrites and absorb unchecked edits (both strategies). `run` warns when a landed change is still another workspace's live working copy
- **Cooperative cancellation** - An ID-scoped durable marker records accepted intent. The owning runner terminates the token-scoped process group and records `canceled`; cancellation and every terminal outcome serialize on `publication.lock`, so a landing that already won its final CAS remains landed.

### Concurrency

- `flock(2)` file locks are used as mutexes
- Lock files stored in `.jj/jjq/locks/` (outside jj's tracked areas), resolved through the shared repo `.jj` directory so invocations from any workspace contend on the same files
- `run` lock ensures only one queue runner at a time
- `meta` lock serializes all metadata-branch writes (sequence IDs, config, workspace records); writers wait up to 15s (`JJQ_META_LOCK_TIMEOUT_MS`) before exiting `LOCK_HELD`
- Metadata reads take no lock: each group of reads pins the `jjq/_/_` bookmark to a single commit and reads every value in the group from that commit
- `check` lock is held shared by `jjq check`; `clean` holds `run` and probes `check` exclusively so it never destroys a live run or check workspace
- `mutation.lock` serializes `push`, `requeue`, `delete`, and `cancel`. `publication.lock` couples queue/history publication, claims, cancellation intent, and every terminal bookmark/history choice.
- Lock order is mutation → publication → meta for producers and run → publication → meta for claims. Mutation paths may acquire `run` only before `publication`; no run path acquires `mutation`. Run → check remains for `clean`, and no code path acquires `run` while holding `meta` or `check`.

### Batch Mode (`run --all`)

When running in batch mode, jjq processes items in sequence:
- Failed items are recorded and processing continues unless `--stop-on-failure` is set
- Canceled items are not failures and do not trigger `--stop-on-failure` or `PARTIAL`
- Summary reports merged and failed counts
- Exits 0 if all items merged or queue was empty; exits 2 (`PARTIAL`) if any items failed
- Exits 1 up front — even with an empty queue — when another runner is active or `check_command` is not configured

## Testing

The project uses Rust integration tests with insta snapshot testing (`tests/e2e.rs`):

```sh
cargo test
```

Tests create temporary jj repositories with pre-built templates (Go projects, multi-PR scenarios, conflict setups) and exercise the full merge queue workflow including conflict handling, batch mode, rebase/merge strategies, exit codes, and idempotent push behavior.

There is also a legacy shell-based e2e script (`jjq-test`) from before the Rust port.

## Development Notes

- jjq is implemented in Rust
- use the Nix flake here for a development shell `nix develop` and to build the artifacts `nix build` -> `./result/bin/jjq`
- All jj interaction is via the `jj` CLI (no API)
- Uses `jj log -T'...'` templates for structured output parsing
- `run_quiet` helper suppresses output on success, shows on failure
- Operations are recorded as commits with trailer metadata on the `jjq/_/_` branch
- Pre-flight conflict check in `push` creates a headless merge commit to test for conflicts without a workspace

## Cutting a Release

1. **Bump version** in `Cargo.toml` (`version = "X.Y.Z"`)
   - The flake.nix reads this dynamically, so no other version files need updating
2. **Run tests** — `cargo test` (all must pass)
3. **Update `CHANGELOG.md`** — Add a section for the new version with categorized notes
4. **Tag and push** — `scripts/tag-release vX.Y.Z`
   - Validates semver format and that tag version matches Cargo.toml
   - Exports jj state to git, creates the git tag, and pushes to origin

GitHub Actions handles the rest automatically:
- Builds tarballs for 4 platforms (aarch64-darwin, x86_64-darwin, x86_64-linux, aarch64-linux) via Nix
- Creates a GitHub release with auto-generated notes
- Updates the Homebrew tap (`paulsmith/homebrew-tap`) with new SHA256 hashes

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
