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
- Creates merge commits combining trunk with each candidate
- Runs configurable checks on each merge
- Only advances trunk when checks pass

This is analogous to CI-based merge queues (like GitHub's), but operates
entirely locally on a single developer's machine with multiple concurrent
lines of work.

## Terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Notational Conventions

- `<placeholder>` denotes a value to be substituted
- `[optional]` denotes optional elements
- Bookmark names are shown in monospace: `jjq/queue/000001`
- Exit codes: 0 indicates success, 1 indicates operational failure,
  2 indicates usage error

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
| `run`    | Active merge workspace           | Zero-padded sequence ID  |
| `lock`   | Mutex-style locks                | Lock name                |
| `_`      | Metadata branch head             | `_` (literal underscore) |

#### Sequence ID Encoding

Sequence IDs in bookmark names MUST be zero-padded to exactly 6 digits.
Valid range: `000001` to `999999`. The value `000000` is reserved and
MUST NOT appear in bookmark names.

Examples of valid bookmarks:
- `jjq/queue/000001` - First queued item
- `jjq/failed/000042` - Failed item 42
- `jjq/run/000007` - Active run for item 7
- `jjq/lock/run` - Queue runner lock
- `jjq/_/_` - Metadata branch head

### Metadata Branch

jjq maintains an isolated branch of revisions for storing metadata. This
branch MUST be parented to the repository's `root()` revision, ensuring
complete isolation from user development history.

The bookmark `jjq/_/_` MUST point to the head (most recent revision) of
this metadata branch.

#### Metadata Branch Contents

The working tree of revisions on the metadata branch contains:

| Path               | Purpose                              | Format        |
|--------------------|--------------------------------------|---------------|
| `last_id`          | Current sequence ID counter          | ASCII integer |
| `config/<key>`     | Configuration values                 | ASCII text    |

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

Defined configuration keys:

| Key              | Default          | Description                        |
|------------------|------------------|------------------------------------|
| `trunk_bookmark` | `main`           | Bookmark designating trunk         |
| `check_command`  | `sh -c 'exit 1'` | Command to validate merge-to-be    |
| `max_failures`   | `3`              | Recent failures shown in status    |

---

## Locking

jjq uses jj bookmarks as mutex-style locks to coordinate concurrent access.
A lock is considered held when its corresponding bookmark exists.

### Lock Acquisition

To acquire a lock, an implementation MUST attempt to create the lock's
bookmark pointing to the `jjq/_/_` revision. If the bookmark already exists,
the lock is held by another process and acquisition MUST fail.

### Lock Release

To release a lock, an implementation MUST delete the lock's bookmark.
Implementations MUST release locks they hold before exiting, including
on error paths.

### Defined Locks

| Bookmark          | Protects                              |
|-------------------|---------------------------------------|
| `jjq/lock/id`     | Sequence ID store read-modify-write   |
| `jjq/lock/run`    | Queue runner exclusivity              |
| `jjq/lock/config` | Configuration store read-modify-write |

#### Sequence ID Lock (`jjq/lock/id`)

MUST be held during the entire read-modify-write cycle of the sequence
ID store. Multiple processes MAY queue items concurrently; this lock
serializes their access to the sequence counter.

#### Run Lock (`jjq/lock/run`)

MUST be held for the duration of a queue run. Only one process MAY
process the queue at a time. This lock MUST be acquired before
processing begins and released after completion (success or failure).

#### Config Lock (`jjq/lock/config`)

MUST be held when reading or writing configuration values. This
serializes concurrent access to the configuration store, preventing
lost updates when multiple processes modify configuration simultaneously.

### Stale Locks

If a process terminates abnormally, locks may remain held. The spec does
not define automatic stale lock detection. Users MAY manually delete
lock bookmarks to recover from this state.

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

### Common Behaviors

- All commands that output messages SHOULD prefix informational messages
  with `jjq: `
- Error messages MUST be written to stderr
- Commands MUST NOT modify user revisions, bookmarks, or working copy
  outside of jjq-namespaced state, except as explicitly specified
  (e.g., moving the trunk bookmark on successful merge)

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
   one revision, the command MUST fail with exit code 1.
2. Ensure jjq is initialized.
3. Acquire the sequence ID lock.
4. Obtain the next sequence ID.
5. Release the sequence ID lock.
6. Create bookmark `jjq/queue/<padded-id>` pointing to the resolved revision.
7. Output confirmation including the assigned sequence ID.
8. Exit with code 0.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Revset does not resolve            | 1         |
| Revset resolves to multiple revs   | 1         |
| Cannot acquire sequence ID lock    | 1         |
| Sequence ID exhausted (at 999999)  | 1         |

#### Notes

- The revision's change ID is captured at push time. If the user amends
  the revision after pushing, the queued bookmark continues to track it
  (jj bookmarks follow change IDs, not commit IDs).
- Pushing the same revision multiple times is permitted; each push
  receives a distinct sequence ID.

---

### run

```
jjq run
```

Process the next item in the queue.

#### Behavior

1. Identify the lowest-numbered queued item by examining bookmarks
   matching `jjq/queue/??????`. If no items exist, output "queue is
   empty" and exit with code 0.
2. Ensure jjq is initialized.
3. Acquire the run lock. If unavailable, MUST fail with exit code 1.
4. Acquire the config lock, read `trunk_bookmark` and `check_command`,
   then release the config lock.
5. Create a temporary workspace outside the repository working copy.
6. Create a merge revision in the workspace with two parents:
   - Parent 1: The revision at the trunk bookmark
   - Parent 2: The queued revision (`jjq/queue/<id>`)
7. Check for conflicts in the merge revision. If conflicts exist:
   - Delete `jjq/queue/<id>`
   - Create `jjq/failed/<id>` pointing to the merge revision
   - Output error message including workspace path
   - Release run lock
   - Exit with code 1 (workspace is NOT deleted)
8. Execute the configured check command in the workspace.
9. If check exits non-zero:
   - Delete `jjq/queue/<id>`
   - Create `jjq/failed/<id>` pointing to the merge revision
   - Output check failure message and workspace path
   - Release run lock
   - Exit with code 1 (workspace is NOT deleted)
10. On success:
    - Delete `jjq/queue/<id>`
    - Move trunk bookmark to the merge revision
    - Delete the temporary workspace
    - Release run lock
    - Exit with code 0

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Run lock unavailable               | 1         |
| Config lock unavailable            | 1         |
| Merge has conflicts                | 1         |
| Check command exits non-zero       | 1         |

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

---

### status

```
jjq status
```

Display the current state of the queue.

#### Behavior

1. If jjq is not initialized, output "not initialized" message and
   exit with code 0.
2. Acquire the config lock, read `max_failures`, then release the
   config lock.
3. Check if the run lock is held. If so, indicate a run is in progress.
4. List all queued items (bookmarks matching `jjq/queue/??????`) in
   ascending sequence ID order.
5. List recent failed items (bookmarks matching `jjq/failed/??????`)
   in descending sequence ID order, limited to `max_failures` entries.
6. If no queued or failed items exist, output "queue is empty".
7. Exit with code 0.

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
  Users requiring full history should use the `log` command.

---

### retry

```
jjq retry <id> [revset]
```

Retry a failed merge attempt by re-queuing it.

#### Arguments

- `<id>`: Sequence ID of the failed item (with or without zero-padding)
- `[revset]`: Optional. A jj revset expression resolving to exactly one
  revision. If omitted, uses the original candidate revision.

#### Behavior

1. Verify `jjq/failed/<padded-id>` exists. If not, fail with exit code 1.
2. If `[revset]` is provided:
   - Resolve it to a revision. If it does not resolve to exactly one
     revision, fail with exit code 1.
   - Use this as the candidate revision.
3. If `[revset]` is omitted:
   - Examine the failed merge revision's parents.
   - The second parent is the original candidate revision.
   - If a user bookmark (non-jjq-namespaced) exists on that revision,
     use the bookmark as the revset.
   - Otherwise, use the change ID directly.
4. Delete `jjq/failed/<padded-id>`.
5. Acquire the sequence ID lock.
6. Obtain the next sequence ID.
7. Release the sequence ID lock.
8. Create bookmark `jjq/queue/<new-padded-id>` pointing to the candidate.
9. Output confirmation including the new sequence ID.
10. Exit with code 0.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Failed item does not exist         | 1         |
| Revset does not resolve            | 1         |
| Revset resolves to multiple revs   | 1         |
| Cannot acquire sequence ID lock    | 1         |
| Sequence ID exhausted (at 999999)  | 1         |

#### Notes

- Retries always receive a new sequence ID, placing them at the end of
  the queue.
- Retries never happen automatically; user intent is always required.

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
   - If yes, delete it and exit with code 0.
4. If neither exists, fail with exit code 1.

#### Errors

| Condition                          | Exit Code |
|------------------------------------|-----------|
| Item not found in queue or failed  | 1         |

#### Notes

- Delete checks the queue first, then failed. An ID cannot exist in
  both simultaneously under normal operation.
- Deleting a failed item does NOT clean up its associated workspace
  (if any). Use `clean` for that.
- Delete does not require any locks; bookmark deletion is atomic in jj.

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

| Key              | Value Type | Description                        |
|------------------|------------|------------------------------------|
| `trunk_bookmark` | string     | Bookmark designating trunk         |
| `check_command`  | string     | Shell command to validate merges   |
| `max_failures`   | integer    | Recent failures shown in status    |

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
3. For `max_failures`, validate value is a non-negative integer.
4. Acquire the config lock.
5. Write value to `config/<key>` on the metadata branch.
6. Release the config lock.
7. Output confirmation.
8. Exit with code 0.

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
modifying the metadata branch and executing merge checks.

### Temporary Workspaces

Workspaces created by jjq MUST be located outside the repository's
working copy tree. This prevents jj from snapshotting workspace
contents into user history.

Implementations SHOULD use the system's temporary directory facility
(e.g., `mktemp -d` or equivalent) to create unique, process-safe
workspace directories.

### Workspace Naming

Workspaces created for queue runs MUST use the naming pattern
`jjq/run/<padded-id>` where the ID corresponds to the queue item
being processed.

Workspaces for other operations (config changes, sequence ID updates)
SHOULD use names that include the process ID or other unique identifier
to avoid collisions with concurrent operations.

### Workspace Lifecycle

| Operation      | On Success          | On Failure                    |
|----------------|---------------------|-------------------------------|
| Queue run      | Delete workspace    | Preserve for debugging        |
| Config change  | Delete workspace    | Delete workspace              |
| Sequence ID    | Delete workspace    | Delete workspace              |

When a workspace is preserved on failure, its path MUST be communicated
to the user via stderr.

### Workspace Cleanup

Implementations MUST call `jj workspace forget` for temporary workspaces
before or after removing the filesystem directory, to prevent stale
workspace entries in jj's workspace list.

---

## Queue Ordering

The jjq queue operates with strict FIFO (first-in, first-out) semantics.

### Ordering Guarantees

- Items are processed in ascending sequence ID order.
- The item with the lowest sequence ID is always processed next.
- Sequence IDs are assigned in the order that `push` (or `retry`)
  commands acquire the sequence ID lock.

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

### Retries and Queue Position

Retried items receive new sequence IDs, placing them at the end of
the queue. This is intentional: a retry represents new user action
and should not jump ahead of items pushed after the original failure.

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
- All bookmark conventions
- All locking requirements

Minimally conforming implementations MAY omit `retry`, `config`, `clean`,
and `log` commands, using hardcoded defaults where configuration would
otherwise apply.

### Interoperability Requirements

Conforming implementations MUST be able to operate on repositories where
jjq state was created by a different conforming implementation. This
requires strict adherence to:

- Bookmark naming conventions (§Data Model - Bookmarks)
- Sequence ID encoding (zero-padded, 6 digits)
- Metadata branch structure (§Data Model - Metadata Branch)
- Lock bookmark semantics (§Locking)

### Extensions

Implementations MAY provide additional commands or features not specified
here. Extensions MUST NOT use the `jjq/` bookmark namespace for purposes
other than those defined in this specification.

Implementations MAY define additional configuration keys. Such keys
SHOULD use a namespaced naming convention (e.g., `impl_name.key`) to
avoid collision with future specification-defined keys.
