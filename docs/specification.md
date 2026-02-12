# jjq Specification

Version: 1.0-draft

## Abstract

jjq is a local merge queue for jj, the Git-compatible VCS. This specification
defines the interface, data formats, and observable behaviors that conforming
implementations MUST exhibit.

## Background (Non-Normative)

This section is informational and does not contain normative requirements.

jjq addresses a common workflow problem: when multiple concurrent changes need
to land on a repository's main trunk, each change may pass its own checks in
isolation but fail when combined with other recent changes. Traditional
solutions require frequent rebasing or risk broken trunk states.

jjq provides a local merge queue that:
- Accepts candidate revisions for merging
- Processes them in FIFO order
- Lands each candidate onto trunk using a configurable strategy (merge
  commit or rebased duplicate)
- Runs configurable checks on each landed candidate
- Only advances trunk when checks pass

This is analogous to CI-based merge queues (like GitHub's), but operates
entirely locally on a single developer's machine with multiple concurrent
lines of work.

## Terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

- **Strategy**: The method jjq uses to land a queued candidate onto trunk.
  Two strategies are defined: merge and rebase.
- **Merge strategy**: Creates a merge commit with two parents (trunk and
  candidate). The merge commit becomes the new trunk head.
- **Rebase strategy**: Duplicates the candidate onto trunk, producing a
  linear commit. The duplicate becomes the new trunk head; the original
  candidate is abandoned when safe.
- **Landing**: The process of integrating a queued candidate revision
  onto trunk, using the configured strategy.

## Notational Conventions

- `<placeholder>` denotes a value to be substituted
- `[optional]` denotes optional elements
- Bookmark names are shown in monospace: `jjq/queue/000001`
- Exit codes: 0 indicates success, 1 indicates operational failure,
  2 indicates partial success (some items succeeded, some failed),
  3 indicates lock contention, 10 indicates usage error

## jj Dependency

jjq operates on jj repositories and requires jj functionality. This
specification describes operations in terms of jj concepts (bookmarks,
revisions, workspaces, change IDs) rather than specific invocation mechanisms.

> **Note:** At the time of writing, the jj CLI with templating is the stable
> interface for programmatic jj interaction. Implementations SHOULD use the
> CLI. Implementations MAY use a jj library API if one becomes stable and
> provides equivalent functionality.

### jj Version Requirements

This specification does not mandate a specific jj version. However,
implementations rely on the following jj features:

- Bookmarks (formerly called "branches" in older jj versions)
- Workspaces
- The `-T` / `--template` option for machine-readable output
- The `root()` revset function
- First-class conflict objects (conflicts stored in tree, queryable
  via the `conflict` template keyword)
- The `duplicate --onto` subcommand (required for rebase strategy)

Implementations SHOULD verify that the available `jj` command supports
these features and report an error if it does not.

---

## Data Model

### Bookmark Conventions

All jjq state is stored using jj bookmarks. All jjq bookmarks MUST be
namespaced with the prefix `jjq/` followed by a scope and details field,
forming the pattern `jjq/<scope>/<details>`.

The three-component structure (`jjq/X/X`) MUST be maintained for all bookmarks.
This ensures compatibility with Git's branch naming (jj exports bookmarks as
Git branches), which treats `/` as directory separators.

#### Defined Scopes

| Scope    | Purpose                          | Details Format           |
|----------|----------------------------------|--------------------------|
| `queue`  | Queued merge candidates          | Zero-padded sequence ID  |
| `failed` | Failed merge attempts            | Zero-padded sequence ID  |
| `_`      | Metadata branch head             | `_` (literal underscore) |

Note: Workspace names use a different pattern (`jjq-run-<padded-id>`) to
avoid confusion with bookmarks. See §Workspaces for details.

Note: Locking uses filesystem-based flock locks, not bookmarks. See §Locking.

#### Sequence ID Encoding

Sequence IDs in bookmark names MUST be zero-padded to exactly 6 digits.
Valid range: `000001` to `999999`. The value `000000` is reserved and
MUST NOT appear in bookmark names.

#### Sequence ID Parsing

When accepting sequence IDs as command arguments (e.g., `delete <id>`),
implementations MUST:

1. Accept only strings consisting entirely of ASCII decimal digits (`0-9`)
2. Reject empty strings, negative numbers, and non-numeric input
3. Interpret the value as a decimal integer (leading zeros are permitted
   and ignored, so `000001`, `01`, and `1` all refer to ID 1)
4. Reject values outside the valid range (less than 1 or greater than 999999)

On parse failure, implementations MUST exit with code 1 and output an
error message indicating the invalid ID.

#### Sequence ID Display

When displaying sequence IDs to users (e.g., in status output, confirmation
messages), implementations SHOULD use the unpadded decimal form (e.g., `1`
not `000001`) for readability. The zero-padded form is only required in
bookmark names.

Examples of valid bookmarks:
- `jjq/queue/000001` - First queued item
- `jjq/failed/000042` - Failed item 42
- `jjq/_/_` - Metadata branch head

### Metadata Branch

jjq maintains an isolated branch of revisions for storing metadata. This
branch MUST be parented to the repository's `root()` revision, ensuring
complete isolation from user development history.

The bookmark `jjq/_/_` MUST point to the head (most recent revision) of
this metadata branch.

#### Metadata Branch Contents

The working tree of revisions on the metadata branch contains:

| Path                    | Purpose                              | Format        |
|-------------------------|--------------------------------------|---------------|
| `last_id`               | Current sequence ID counter          | ASCII integer |
| `config/<key>`          | Configuration values                 | ASCII text    |
| `log_hint_shown`        | Log filter hint display marker       | ASCII text    |

##### Sequence ID Store (`last_id`)

The file `last_id` MUST contain a single ASCII decimal integer representing
the most recently assigned sequence ID. Initial value MUST be `0`.

When a new sequence ID is required, implementations MUST:
1. Acquire the sequence ID lock (see §Locking)
2. Read the current value from `last_id`
3. If the current value is `999999`, release the lock and fail with an
   error indicating sequence ID exhaustion
4. Increment by 1
5. Write the new value to `last_id`
6. Use the new value as the sequence ID
7. Release the lock

The sequence ID `0` is never assigned; the first assigned ID is `1`.
The maximum assignable sequence ID is `999999`.

Gaps in assigned sequence IDs MAY occur. For example, if a `push` operation
obtains a sequence ID but fails before creating the queue bookmark, that
ID is consumed but no corresponding queue item exists. Implementations
MUST NOT assume sequence IDs are contiguous.

##### Configuration Store (`config/`)

Configuration values are stored as individual files under `config/`.
Each file contains the configuration value as plain text (single line,
no trailing newline required but permitted).

Lazy initialization (§Initialization) MUST NOT create config files. A
missing config file means the default value applies. Config files are
created when the user explicitly sets values via the `init` or `config`
commands.

When reading configuration, implementations MUST:
1. Attempt to read `config/<key>` from the metadata branch
2. If the file exists, use its contents as the value
3. If the file does not exist, use the default value (if any)

Defined configuration keys:

| Key              | Default   | Description                          |
|------------------|-----------|--------------------------------------|
| `trunk_bookmark` | `main`    | Bookmark designating trunk           |
| `check_command`  | (none)    | Command to validate merge-to-be      |
| `strategy`       | `merge`   | Landing strategy (`merge` or `rebase`) |

The `check_command` key has no default. If unconfigured, the `run` command
MUST fail with an error indicating that configuration is required.

The `strategy` key accepts only the values `merge` or `rebase`.
Implementations MUST reject any other value when setting this key. If
the key is absent (e.g., repositories initialized before strategy support
was added), the default value `merge` applies, preserving backward
compatibility.

##### Log Hint Marker (`log_hint_shown`)

The file `log_hint_shown` is a marker indicating that the log filter hint
has been displayed to the user. Its presence (not its contents) determines
whether the hint should be shown.

- If the file exists: the hint MUST NOT be shown
- If the file does not exist: the hint MAY be shown (subject to other
  conditions; see §Initialization - Log Filter Hint)

This file is created automatically when the hint is displayed and MUST NOT
be created during initialization.

---

## Locking

jjq uses flock-based file locks to coordinate concurrent access. Each
lock is a file in `.jj/jjq-locks/` that is locked using the operating
system's advisory file locking mechanism (flock). The OS automatically
releases locks when the holding process exits, even abnormally.

> **Note:** Earlier versions of this specification described bookmark-based
> locking (replaced because jj's optimistic concurrency does not guarantee
> mutual exclusion) and mkdir-based locking (replaced because stale lock
> directories required manual cleanup after crashes). See
> `docs/jj-transaction-model.md` for details on why bookmark locking is
> unsuitable.

### Lock Storage

All locks are stored in the `.jj/jjq-locks/` directory within the repository.
Each lock is a file named `<lock-name>.lock`.

```
<repo>/.jj/jjq-locks/
├── id.lock      # Sequence ID lock
└── run.lock     # Queue runner lock
```

### Lock Acquisition

To acquire a lock, an implementation MUST:

1. Open (or create) the lock file at `.jj/jjq-locks/<name>.lock`
2. Attempt an exclusive flock on the file handle
3. If the flock succeeds, the lock is acquired
4. If the flock fails (lock held by another process), acquisition
   MUST fail

### Lock Release

Locks are released by closing the file handle (or by dropping the
lock guard, depending on implementation). Implementations MUST release
locks they hold before exiting, including on error paths. If the
process exits abnormally (crash, signal), the OS releases the lock
automatically.

### Defined Locks

| Lock Name | Protects                              |
|-----------|---------------------------------------|
| `id`      | Sequence ID store read-modify-write   |
| `run`     | Queue runner exclusivity              |

#### Sequence ID Lock (`id`)

MUST be held during the entire read-modify-write cycle of the sequence
ID store. Multiple processes MAY queue items concurrently; this lock
serializes their access to the sequence counter.

#### Run Lock (`run`)

MUST be held for the duration of a queue run. Only one process MAY
process the queue at a time. This lock MUST be acquired before
processing begins and released after completion (success or failure).

### Lock Probing

Implementations MAY probe lock state (e.g., for `status` display) by
attempting a non-blocking flock. The result is either Free or Held;
no information about the lock holder (such as a PID) is available.

### Stale Locks

Stale locks cannot occur under normal operation. The OS releases flock
locks when the holding process exits, regardless of whether the exit
is clean or abnormal. Lock files may remain on disk after release, but
an empty lock file does not indicate a held lock.

---

## Commands

### Repository Precondition

jjq MUST be invoked within a jj repository (a directory containing a `.jj`
subdirectory, or a subdirectory thereof). If invoked outside a jj repository,
implementations MUST output an error message to stderr and exit with code 1.

### Invocation

jjq is invoked as:

```
jjq <command> [arguments...]
```

If invoked with no command or an unrecognized command, implementations
MUST print a usage message to stderr and exit with code 2.

### Initialization

jjq state (the metadata branch, `jjq/_/_` bookmark, and sequence ID store)
is created lazily on first use. Commands that modify state MUST ensure
initialization before proceeding. Commands that only read state (e.g.,
`status`) MAY report "not initialized" if the `jjq/_/_` bookmark does
not exist.

Initialization MUST:
1. Create a new revision parented to `root()`
2. Create the `last_id` file with contents `0`
3. Create the `jjq/_/_` bookmark pointing to this revision

Initialization MUST be idempotent - if already initialized, it MUST
succeed without modification.

#### Log Filter Hint

After initialization, implementations SHOULD display a one-time hint
suggesting how to hide jjq metadata from `jj log` output. This hint
helps users configure their repository to exclude the `jjq/_/_` branch
from normal log views.

The hint MUST only be shown when ALL of the following conditions are met:
1. Standard output is connected to a terminal (TTY detection)
2. The jj `revsets.log` configuration does not already reference the
   jjq metadata bookmark
3. The `log_hint_shown` marker file does not exist in the metadata branch

When shown, the hint SHOULD suggest running:
```
jj config set --repo revsets.log '~ ::jjq/_/_'
```

Implementations SHOULD also configure this filter automatically during
initialization (`init` command).

After displaying the hint, implementations MUST create the `log_hint_shown`
file in the metadata branch to prevent the hint from appearing again.
The file contents are not significant; its presence is the marker.

This hint-based approach ensures:
- Scripts and non-interactive contexts are not blocked by prompts
- Users are informed of the configuration option
- The hint appears only once per repository

### Common Behaviors

- All commands that output messages SHOULD prefix informational messages
  with `jjq: `
- Error messages MUST be written to stderr
- Commands MUST NOT modify user revisions, bookmarks, or working copy
  outside of jjq-namespaced state, except as explicitly specified
  (e.g., moving the trunk bookmark on successful landing)

---

### init

```
jjq init [--trunk <name>] [--check <command>] [--strategy <strategy>]
```

Initialize jjq in this repository.

#### Options

- `--trunk <name>`: Trunk bookmark name. If omitted, detected from
  existing bookmarks or prompted interactively.
- `--check <command>`: Check command. If omitted, prompted interactively.
- `--strategy <strategy>`: Landing strategy. Default: `rebase`. Valid
  values: `merge`, `rebase`.

#### Behavior

1. If jjq is already initialized, MUST fail with exit code 10.
2. Determine trunk bookmark, check command, and strategy from flags
   or interactive prompts. In non-interactive mode (no TTY), `--trunk`
   and `--check` are REQUIRED.
3. Perform lazy initialization (create metadata branch, see
   §Initialization).
4. Set `trunk_bookmark`, `check_command`, and `strategy` configuration
   values on the metadata branch.
5. Output confirmation showing effective configuration.
6. Exit with code 0.

#### Notes

- The `init` command always writes the `strategy` config value. This
  is distinct from lazy initialization, which MUST NOT create config
  files. The `init` command is an explicit user action that establishes
  the repository's configuration.
- The default strategy for `init` is `rebase`. Existing repositories
  that were initialized before strategy support (and thus have no
  `strategy` config key) default to `merge` for backward compatibility
  (see §Configuration Store).

---

### push

```
jjq push <revset>
```

Queue a revision for merging to trunk.

#### Arguments

- `<revset>`: A jj revset expression resolving to exactly one revision

#### Behavior

1. Resolve `<revset>` to a revision. If it does not resolve to exactly
   one revision, the command MUST fail with exit code 10.
2. Resolve the revision's change ID and commit ID.
3. Idempotent cleanup:
   a. Scan all `jjq/queue/*` bookmarks. If any points to the same
      commit ID, MUST fail with exit code 10 ("already queued at N").
   b. If any queue bookmark points to the same change ID but a
      different commit ID, delete it and inform the user
      ("replacing queued entry N").
   c. Scan all `jjq/failed/*` bookmarks. For each, extract the
      candidate change ID from the `jjq-candidate:` trailer in its
      commit description. If it matches, delete the failed bookmark
      and inform the user ("clearing failed entry N").
4. Perform a pre-flight conflict check:
   - Create a headless merge commit with the trunk bookmark and the
     candidate revision as parents
   - Check if the merge revision has conflicts
   - Abandon the temporary merge commit
   - If conflicts exist, MUST fail with exit code 1 and output an error
     message indicating the revision conflicts with trunk
5. Ensure jjq is initialized.
6. Acquire the sequence ID lock.
7. Obtain the next sequence ID.
8. Release the sequence ID lock.
9. Create bookmark `jjq/queue/<padded-id>` pointing to the resolved revision.
10. Output confirmation including the assigned sequence ID.
11. Exit with code 0.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Revset does not resolve            | 10        |
| Revset resolves to multiple revs   | 10        |
| Exact duplicate (same commit ID)   | 10        |
| Trunk bookmark does not exist      | 10        |
| Revision conflicts with trunk      | 1         |
| Cannot acquire sequence ID lock    | 3         |
| Sequence ID exhausted (at 999999)  | 10        |

#### Notes

- Push is idempotent over change IDs: re-pushing the same change ID
  (with a different commit ID) replaces any existing queue or failed
  entries for that change. This is the intended workflow for handling
  failures — fix the revision, rebase onto trunk, push again.
- The conflict check verifies compatibility with trunk at push time.
  The check uses a temporary merge commit regardless of the active
  strategy (merge conflicts and rebase conflicts are equivalent in jj).
  However, trunk may advance between push and run, so a clean push
  does not guarantee a clean landing at run time. The check catches
  conflicts that exist at the moment of pushing.

---

### run

```
jjq run [--all] [--stop-on-failure]
```

Process the next item in the queue, or all items if `--all` is specified.

#### Options

- `--all`: Process all queued items in sequence until the queue is empty.
  Failed items are moved to the failed list and processing continues with
  the next item. On completion, outputs a summary of items processed and
  failures.
- `--stop-on-failure`: Only meaningful with `--all`. Stops processing at
  the first failure instead of continuing to the next item.

#### Behavior

**Single item mode (default):**

1. Identify the lowest-numbered queued item by examining bookmarks
   matching `jjq/queue/??????`. If no items exist, output "queue is
   empty" and exit with code 0.
2. Ensure jjq is initialized.
3. Acquire the run lock. If unavailable, MUST fail with exit code 1.
4. Acquire the config lock, read `trunk_bookmark` and `check_command`,
   then release the config lock.
5. Create a temporary workspace directory outside the repository working copy.
6. Record the current trunk revision (commit ID) for later verification.
7. Read the `strategy` configuration value. Create the landed revision
   and workspace according to the active strategy:

   Before creating the workspace, the candidate's change ID and commit
   description MUST be captured from the queue bookmark (for use in
   failure and success descriptions).

   **Merge strategy:**
   Create a jj workspace named `jjq-run-<padded-id>` at that directory,
   with a new merge revision having two parents in this order:
   - Parent 1 (first): The revision at the trunk bookmark
   - Parent 2 (second): The queued revision (`jjq/queue/<id>`)

   The merge revision is the working copy of this workspace.

   **Rebase strategy:**
   Duplicate the queued revision onto the trunk bookmark revision using
   `jj duplicate <candidate> --onto <trunk>`, creating a rebased copy.
   Parse the new change ID from the command output. Create a jj workspace
   named `jjq-run-<padded-id>` at that directory with `-r <duplicate>`.
   Then `jj edit <duplicate>` in the workspace context so the duplicate
   itself (not a child commit) is the workspace's working copy. This
   ensures that check command artifacts are snapshotted into the
   duplicate, matching merge strategy behavior.

   In both strategies, the workspace working copy can be referenced as
   `jjq-run-<padded-id>@` in revsets.
8. Check for conflicts in the workspace working copy revision (merge
   revision under merge strategy, duplicate revision under rebase
   strategy). A revision has conflicts if its tree contains conflict
   objects (in jj terms, the `conflict` template keyword evaluates to
   true). If conflicts exist:
   - Delete `jjq/queue/<id>`
   - Create `jjq/failed/<id>` pointing to the conflicted revision
   - Set the revision's description to include failure trailers (see
     §Failure Description Trailers)
   - Output error message including workspace path
   - Actionable guidance SHOULD be displayed telling the user how to
     resolve (e.g., rebase onto trunk, resolve conflicts, and use
     `jjq push`)
   - Release run lock
   - Exit with code 1 (workspace is NOT deleted)
9. Execute the configured check command in the workspace.
10. If check exits non-zero:
    - Delete `jjq/queue/<id>`
    - Create `jjq/failed/<id>` pointing to the workspace working copy
      revision
    - Set the revision's description to include failure trailers (see
      §Failure Description Trailers), with `jjq-failure: check`
    - Output check failure message and workspace path
    - Actionable guidance SHOULD be displayed telling the user how
      to resolve (e.g., fix the issue and use `jjq push`)
    - Release run lock
    - Exit with code 1 (workspace is NOT deleted)
11. Verify trunk bookmark still points to the same revision recorded in
    step 6. If trunk has moved:
    - Leave `jjq/queue/<id>` in place (do NOT delete it)
    - Under the rebase strategy, abandon the duplicate revision created
      in step 7 (it is no longer needed; a new duplicate will be created
      on retry). Under the merge strategy, the merge revision is
      cleaned up when the workspace is forgotten.
    - Forget the workspace and delete the workspace directory
    - Release run lock
    - Output error message indicating trunk moved during run
    - Exit with code 1
12. On success, behavior depends on the active strategy:

    **Merge strategy:**
    - Capture the merge revision's change ID (before any modifications)
    - Delete `jjq/queue/<id>`
    - Move trunk bookmark to the merge revision (`jjq-run-<padded-id>@`)
    - Forget the workspace (`jj workspace forget jjq-run-<padded-id>`)
    - Delete the temporary workspace directory
    - Output success message including sequence ID, trunk bookmark name,
      and the merge revision's change ID
    - Release run lock
    - Exit with code 0

    **Rebase strategy:**
    The duplicate was used only for testing. Now rebase the ORIGINAL
    candidate to preserve its change ID:
    - Rebase the original candidate (and its ancestors up to trunk, plus
      any descendants) onto trunk using `jj rebase -b <candidate> -d <trunk>`
    - Move trunk bookmark to the rebased original candidate (this is the
      first and most critical operation for crash safety; the change ID
      is preserved)
    - Delete `jjq/queue/<id>`
    - Describe the rebased candidate with the original commit description
      plus trailers:
      ```
      <original description>

      jjq-sequence: <sequence_id>
      jjq-strategy: rebase
      ```
    - Abandon the duplicate (it was only used for testing)
    - Forget the workspace (`jj workspace forget jjq-run-<padded-id>`)
    - Delete the temporary workspace directory
    - Output success message including sequence ID, trunk bookmark name,
      and the candidate's change ID (which is preserved)
    - Release run lock
    - Exit with code 0

**Batch mode (`--all`):**

When invoked with `--all`, the run command processes items repeatedly
until the queue is empty:

1. Execute the single item behavior (steps 1-12 above).
2. If the item succeeded (step 12), loop back to step 1.
3. If the item failed (steps 8, 10, or 11):
   - If `--stop-on-failure` is set, output a summary of items processed
     before the failure (if any) and exit with code 1.
   - Otherwise, continue to step 1 (process the next item).
4. If the queue was empty (step 1), output a summary of items processed
   and failures (if any):
   - If all items succeeded, exit with code 0.
   - If some items succeeded and some failed, exit with code 2.

The run lock is acquired and released for each individual item, not
held for the entire batch. This allows inspection of intermediate
states between items if needed.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| check_command not configured       | 1         |
| Run lock unavailable               | 1         |
| Config lock unavailable            | 1         |
| Landed revision has conflicts      | 1         |
| Check command exits non-zero       | 1         |
| Trunk moved during run             | 1         |
| Partial success (--all, no --stop-on-failure) | 2 |

#### Workspace Behavior

The temporary workspace MUST be created in a location outside the
repository's working copy tree (e.g., using the system temporary
directory). This prevents jj from snapshotting workspace contents
into the user's repository.

On failure (conflict or check failure), the workspace MUST be preserved
and its path output to the user for debugging purposes.

On success, the workspace MUST be removed.

#### Notes

- Only one `run` process may execute at a time due to the run lock.
- The check command is executed via a POSIX shell. Its exit code
  determines success (0) or failure (non-zero).
- The check command's stdout/stderr SHOULD be suppressed on success
  and displayed on failure.
- The trunk bookmark is only moved after successful completion of
  all steps. A failure at any point leaves trunk unchanged.
- If trunk moves during a run (e.g., user manually advances it), the
  queue item is left in place and the user can simply re-run. This
  avoids discarding commits that were added to trunk during the run.
- The strategy may be changed between runs (e.g., via
  `jjq config strategy rebase`). Queued items are strategy-agnostic
  bookmarks; the strategy is read at the start of each run. Existing
  failed items retain their original strategy's artifacts.

#### Landed Revision Lifecycle

The revision created in step 7 differs by strategy but follows the same
lifecycle pattern regardless of whether the run succeeds or fails:

**Merge strategy:**
The merge revision is a proper merge commit with two parents (trunk and
candidate).

- **On success**: The merge revision becomes the new trunk head.
- **On failure**: The merge revision remains in the DAG as a side branch.
  The `jjq/failed/<id>` bookmark prevents garbage collection, allowing
  users to inspect it for debugging.

**Rebase strategy:**
A duplicate revision is created for testing (linear commit with trunk as
parent). The original candidate is not modified during testing.

- **On success**: The original candidate is rebased onto trunk (preserving
  its change ID) and becomes the new trunk head, producing linear history.
  The duplicate is abandoned. Any descendants of the candidate are rebased
  along with it.
- **On failure**: The duplicate remains in the DAG. The
  `jjq/failed/<id>` bookmark prevents garbage collection, allowing
  users to inspect it for debugging. The original candidate is
  untouched; the user fixes and re-pushes.

In both strategies, the landed revision is NOT parented to `root()` -
it is part of the repository's normal commit history, just not reachable
from trunk until/unless it succeeds.

#### Failure Description Trailers

When a queue item fails (conflicts or check failure), the landed
revision's description MUST be set to a structured format containing
trailers that identify the failure context. The description MUST use
the following format:

```
Failed: merge <id> (<reason>)

jjq-candidate: <change_id>
jjq-candidate-commit: <commit_id>
jjq-trunk: <trunk_commit_id>
jjq-workspace: <workspace_path>
jjq-failure: conflicts|check
jjq-strategy: rebase|merge
```

Note: The summary line uses the word "merge" regardless of the active
strategy. This refers to the queue item number (historically called a
"merge"), not the landing strategy. The `jjq-strategy` trailer carries
the actual strategy used.

Where:
- `<id>` is the sequence ID
- `<reason>` is `conflicts` or the check failure description
- `jjq-candidate` is the change ID of the original candidate revision
- `jjq-candidate-commit` is the commit ID of the original candidate
- `jjq-trunk` is the commit ID of the trunk revision at the time of
  the run
- `jjq-workspace` is the filesystem path to the preserved workspace
- `jjq-failure` is either `conflicts` or `check`
- `jjq-strategy` is either `rebase` or `merge`

The `jjq-candidate` trailer is the primary key used to associate failed
items back to their original candidate revisions (e.g., for idempotent
re-push). The `jjq-strategy` trailer MAY be absent for backward
compatibility; if absent, `merge` is assumed.

---

### status

```
jjq status
```

Display the current state of the queue.

#### Behavior

1. If jjq is not initialized, output "not initialized" message and
   exit with code 0.
2. Check if the run lock is held. If so, indicate a run is in progress.
3. List all queued items (bookmarks matching `jjq/queue/??????`) in
   ascending sequence ID order.
4. List all failed items (bookmarks matching `jjq/failed/??????`)
   in descending sequence ID order.
5. If no queued or failed items exist, output "queue is empty".
6. Exit with code 0.

#### Output

For each queued or failed item, implementations SHOULD display:
- The sequence ID (without zero-padding)
- The short change ID of the revision
- The first line of the revision's description

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Config lock unavailable            | 1         |

An uninitialized repository is not an error.

#### Notes

- Status is read-only except for the config lock acquisition.
- Status is intended as a quick overview, not a comprehensive history.

---

### delete

```
jjq delete <id>
```

Remove an item from the queue or failed list.

#### Arguments

- `<id>`: Sequence ID of the item (with or without zero-padding)

#### Behavior

1. Ensure jjq is initialized.
2. Check if `jjq/queue/<padded-id>` exists:
   - If yes, delete it and exit with code 0.
3. Check if `jjq/failed/<padded-id>` exists:
   - If yes, delete the bookmark.
   - Look up the workspace named `jjq-run-<padded-id>`. If it is
     registered, forget it via `jj workspace forget`.
   - If the workspace directory still exists on the filesystem,
     remove it.
   - Output the removed workspace path (if applicable).
   - Exit with code 0.
4. If neither exists, fail with exit code 1.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Item not found in queue or failed  | 1         |

#### Notes

- Delete checks the queue first, then failed. An ID cannot exist in
  both simultaneously under normal operation.
- Deleting a failed item also cleans up its associated workspace
  (if any), including forgetting the workspace in jj and removing
  the workspace directory from the filesystem.
- Delete does not require any locks; bookmark deletion is atomic in jj.

---

### clean

```
jjq clean
```

Remove all jjq workspaces and their directories.

#### Behavior

1. Enumerate all jj workspaces matching the pattern `jjq-run-*`.
2. If no matching workspaces exist, output "no workspaces to clean"
   and exit with code 0.
3. For each matching workspace:
   - Extract the sequence ID from the workspace name.
   - Determine if a corresponding `jjq/failed/<padded-id>` bookmark
     exists (label the workspace as "failed item N") or not (label
     as "orphaned").
   - Forget the workspace via `jj workspace forget`.
   - If the workspace directory exists on the filesystem, remove it.
4. Output summary: count of removed workspaces with per-workspace
   details (name, label, path if known).
5. Exit with code 0.

#### Notes

- The `clean` command acts immediately without confirmation.
- Workspace filesystem paths are resolved from the jjq metadata
  branch operation log.
- `clean` does NOT delete `jjq/failed/*` bookmarks; it only removes
  workspaces. Use `delete` to remove failed items.

---

### config

```
jjq config [key] [value]
```

Get or set configuration values.

#### Arguments

- `[key]`: Configuration key. If omitted, display all configuration.
- `[value]`: Value to set. If omitted with key, display that key's value.

#### Valid Keys

| Key              | Value Type | Description                          |
|------------------|------------|--------------------------------------|
| `trunk_bookmark` | string     | Bookmark designating trunk           |
| `check_command`  | string     | Shell command to validate merges     |
| `strategy`       | string     | Landing strategy (`merge` or `rebase`) |

#### Behavior

**No arguments (`jjq config`):**
1. Ensure jjq is initialized.
2. Acquire the config lock.
3. Display all configuration keys with their effective values
   (stored value or default).
4. Release the config lock.
5. Exit with code 0.

**Key only (`jjq config <key>`):**
1. Validate key is recognized. If not, fail with exit code 1.
2. Acquire the config lock.
3. Display the effective value for that key.
4. Release the config lock.
5. Exit with code 0.

**Key and value (`jjq config <key> <value>`):**
1. Ensure jjq is initialized.
2. Validate key is recognized. If not, fail with exit code 1.
3. Acquire the config lock.
4. Write value to `config/<key>` on the metadata branch.
5. Release the config lock.
6. Output confirmation.
7. Exit with code 0.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Unknown configuration key          | 1         |
| Invalid value for key type         | 1         |
| Cannot acquire config lock         | 1         |

---

### log

```
jjq log [limit]
```

Display jjq operation history.

#### Arguments

- `[limit]`: Maximum number of entries to display. Default: 20.

#### Behavior

TK TK

---

## Workspaces

jjq uses jj workspaces for operations that require a working copy:
modifying the metadata branch and executing landing checks.

Note that the workspace name in a jj sense is a bookmark-like identification of
a working copy and its revisions. This is different than the directory name in
the file path of the working copy in a filesystem. Take care to distinguish the
two.

### Temporary Workspaces

Workspaces created by jjq MUST be located outside the repository's
working copy tree. This prevents jj from snapshotting workspace
contents into user history.

Implementations SHOULD use the system's temporary directory facility
(e.g., `mktemp -d` or equivalent) to create unique, process-safe
workspace directories.

### Workspace Naming

Workspaces created for queue runs MUST use the naming pattern
`jjq-run-<padded-id>` where the ID corresponds to the queue item
being processed. This pattern uses hyphens rather than slashes to
distinguish workspace names from jjq bookmark names.

Workspaces for other operations (config changes, sequence ID updates)
SHOULD use names that include the process ID or other unique identifier
to avoid collisions with concurrent operations.

### Workspace Lifecycle

| Operation      | On Success          | On Failure                    |
|----------------|---------------------|-------------------------------|
| Queue run      | Forget and delete   | Preserve (keep registered)    |
| Config change  | Forget and delete   | Forget and delete             |
| Sequence ID    | Forget and delete   | Forget and delete             |

When a workspace is preserved on failure, its path MUST be communicated
to the user via stderr.

### Workspace Cleanup

For workspaces being removed, implementations MUST call `jj workspace forget`
before or after removing the filesystem directory, to prevent stale
workspace entries in jj's workspace list.

For preserved workspaces (failed queue runs), implementations MUST NOT call
`jj workspace forget`. Keeping the workspace registered allows users to
inspect the failed merge using jj commands (e.g., `jj log -r 'jjq-run-000001@'`
or `jj diff -r 'jjq-run-000001@'`). Users can manually forget and remove
the workspace after debugging.

---

## Queue Ordering

The jjq queue operates with strict FIFO (first-in, first-out) semantics.

### Ordering Guarantees

- Items are processed in ascending sequence ID order.
- The item with the lowest sequence ID is always processed next.
- Sequence IDs are assigned in the order that `push` commands acquire
  the sequence ID lock.

### Determining Queue Order

To identify the next item to process, implementations MUST:
1. Enumerate all bookmarks matching `jjq/queue/??????`
2. Extract the sequence ID from each bookmark name
3. Select the bookmark with the numerically lowest sequence ID

### Concurrent Push Behavior

When multiple processes push items concurrently:
- Each acquires the sequence ID lock in turn
- Each receives a unique, monotonically increasing sequence ID
- The order of sequence IDs reflects the order of lock acquisition,
  which may differ from the order the commands were initiated

### Re-pushed Items and Queue Position

When a failed item is fixed and re-pushed, the new push receives a new
sequence ID, placing it at the end of the queue. This is intentional:
a re-push represents new user action and should not jump ahead of items
pushed after the original failure.

### No Priority or Reordering

jjq does not support priority levels or manual reordering. The only
way to change effective queue order is to delete items and re-push
them in the desired order.

---

## Conformance

A conforming jjq implementation MUST satisfy all normative requirements
in this specification.

### Conformance Levels

**Fully Conforming:** Implements all commands and behaviors specified
in this document.

**Minimally Conforming:** Implements at least:
- `push` command
- `run` command
- `status` command
- `delete` command
- `config` command
- All bookmark conventions
- All locking requirements

### Interoperability Requirements

Conforming implementations MUST be able to operate on repositories where
jjq state was created by a different conforming implementation. This
requires strict adherence to:

- Bookmark naming conventions (§Data Model - Bookmarks)
- Sequence ID encoding (zero-padded, 6 digits)
- Metadata branch structure (§Data Model - Metadata Branch)
- Lock file semantics (§Locking)

### Extensions

Implementations MAY provide additional commands or features not specified
here. Extensions MUST NOT use the `jjq/` bookmark namespace for purposes
other than those defined in this specification.

Implementations MAY define additional configuration keys. Such keys
SHOULD use a namespaced naming convention (e.g., `impl_name.key`) to
avoid collision with future specification-defined keys.
