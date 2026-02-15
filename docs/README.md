# jjq - merge queue for jj

jjq is a merge queue tool for jj (Jujutsu), the Git-compatible VCS. jjq is
lightweight, in that it can be used on a jj repo with little ceremony or
disruption and coexist with other processes. jjq is a jj-native merge queue
tool, in that it uses jj features such as bookmarks and workspaces to operate
the merge queue. jjq uses the jj repo as a data store for its queue items and
metadata, while isolating its data from the user's history.

jjq is a CLI. Users issue jjq commands like `push`, `run`, and `status` to
operate the merge queue and get information about it.

jjq is intended as a local merge queue tool, one that operates on a repository
on a single computer which may have multiple concurrent changes that need to be
merged.

The point of jjq is the same as other merge queue implementations such as
GitHub's: it allows for concurrent and parallel development on a repository to
land on the main trunk while minimizing rebasing, and preventing accidental
merges that passed checks in feature branches but not when brought up to date
with recent changes to the trunk.

## Notes

As per jj documentation, the terms "revision" and "commit" may be used
interchangeably throughout this document, although "revision" is often
preferred, as it is a clearer jj-specific concept than "commit", which can mean
either a Git commit (eg., if the storage backend of the jj repo is Git) or the
immutable version related to a change ID, which is the stable ID in jj as
revisions are mutated.

## Design

jjq operates in an existing jj repository that is storing the versions of a
user's source code. jjq stores metadata about the user's intent to merge
candidate revisions directly in the same jj repo.

### The queue

The jjq merge queue is a set of candidate revisions to be landed on the
repository's trunk (more on that later). The landing strategy is configurable:
with the `merge` strategy, a new commit with two parents (the current trunk
revision and the candidate revision) is created; with the `rebase` strategy
(default for new repos), the candidate is duplicated onto trunk to produce
linear history, preserving the original change ID. The trunk is a revision in
the repo that has passed a check (more on checks later). The queue is processed
in FIFO order.

### Pushing to the queue

Candidate revisions to be merged are "pushed" to the queue by the user of the
jjq CLI. Users indicate a revision (eg., a short change ID or other string in
the jj revset language, so long as that revset refers to a single revision) to
be queued, and jjq does the following:

  - finds the next sequence ID (see "Sequence IDs" below);
  - creates a bookmark namespaced to jjq and encoding the sequence ID; and
  - points the bookmark to the candidate revision.

### Sequence IDs

jjq maintains a monotonically-increasing singleton sequence ID in the jj repo.
The sequence ID starts at 0 for a new jjq-using repo. When a candidate revision
is pushed to the queue or retried after a failure, jjq reads the current value
of the sequence ID from its store, increments it by 1, writes the new value
back to the sequence ID store, and uses the new ID for its operation. Therefore
0 is never a valid sequence ID for a queued item.

Sequence IDs are encoded in jjq-namespaced bookmarks by zero-padding to a width
of 6, therefore, a maximum of 999999 sequence IDs are possible.

### The trunk

A jj repository's trunk is, in the context of jjq, a revision that has passed a
check that indicates that revision has met some criteria that makes it suitable
to be considered the current "blessed" revision of the source code. For
example, trunk could have passed a CI check that runs all tests and lints and
is considered safe to deploy by the operations team.

jjq indicates the trunk using a jj bookmark. This bookmark defaults to "main".
Users may configure jjq to use any local bookmark they choose. Only local
bookmarks are allowed because jjq will update the bookmark to point to the
latest trunk after a successful merge.

### Running the queue

A jjq queue run is started by the user via the CLI. One queue item is processed
at a time. Future versions of jjq may support batching or continuous running.

A queue run begins by determining the current lowest-numbered queue item. Since
queue items are numbered with the sequence ID which is monotonically-increasing,
this enforces the FIFO ordering of the queue.

The lowest-numbered queue item is found by querying the jj bookmark list and
filtering for the jjq-namespaced bookmarks.

An empty queue is a normal condition and is a no-op for a jjq run.

### Concurrency

Because jjq is meant to support multiple changes on a single jj repo, it must
ensure that certain operations are safe from concurrent access.

The sequence ID store is protected by an OS file lock (`flock(2)`) on
`.jj/jjq-locks/id.lock`.

Running the queue also takes a lock — only one merge can be attempted at a
time. The `run` lock uses `.jj/jjq-locks/run.lock`.

Configuration reads/writes are serialized via `.jj/jjq-locks/config.lock`.

### Landed revision

The "landed revision" is the revision that combines trunk and the candidate.
Under the merge strategy it is a merge commit with two parents; under the
rebase strategy it is a duplicate of the candidate rebased onto trunk. It
doesn't exist until a jjq run is executed.

### Conflicts

Combining the trunk and the candidate revision can lead to conflicts, regardless
of which strategy is in use.

Conflicts are a first-class concept in jj, in that they are stored as objects
in the repo, and don't prevent subsequent operations.

A conflict in the landed revision will mark that queue item as having failed.

### Checks

A check in jjq is the command to be run on the landed revision — the commit
that combines the trunk and the candidate revision.

The check can be any command that can be executed by a POSIX shell. Its exit
code, zero (success) or non-zero (failure), is determinative of the success of
the check.

### Workspaces

jjq uses a jj workspace during a run for the working copy to produce the
landed revision and to execute the check on same.

This "runner" workspace shall be located outside the working copy of the jj
repo, so as not to accidentally be snapshotted by jj (an alternative might be to
allow something like <repo>/.workspaces/ and add .workspaces to the .gitignore,
but this would conflict with the design goal of minimization, see below).

jjq shall use the /tmp directory or similar (`mktemp -d`) to get a unique and
process-safe directory for the runner workspace.

On failure of a merge queue run, the runner workspace shall not be deleted, and
its path printed to the user, such that they may use any artifacts located
within as forensics for their debugging.

### Minimizing visible artifacts

jjq has a design goal of minimizing the artifacts it needs to work from being
visible to the user as much as possible, so as not to disrupt the regular
non-jjq use of the repo.

For example, jjq tries to avoid destructive edits user revisions, like appending
to .gitignore.

jjq-namespaced bookmarks are visible through commands like `jj log`, which is
acceptable.

One way jjq accomplishes this minimization is by having its own "branch" of
revisions, parented to the 'root()' commit, that is isolated from the user's
regular timelines of development. This allows jjq to have a working copy of its
own for things like the sequence ID store, which can just be a regular file,
that doesn't "pollute" any user-owned working copy. This branch can be hidden
from `jj log` by adding:

```
[revsets]
log = "~ ::jjq/_/_"
```

to the repo's local config.toml. Note that the bookmark `jjq/_/_` is a special
bookmark that points to the head of the isolated branch.

### Failed merge attempts

When a merge attempt fails (conflict or check failure), that queue item is
marked as "failed" with a bookmark `jjq/failed/NNNNNN` pointing at the
conflicted or failed merge commit. The runner workspace directory is preserved
for debugging.

To resolve, fix the revision (rebase onto trunk, resolve conflicts) and push it
again. Push is idempotent — re-pushing the same change ID clears any stale
queue or failed entries for that change and queues the updated revision. If you
re-push the exact same commit ID that is already queued, the push is rejected
as a duplicate.

### Status

jjq users can get a list of the current queue via the CLI's status command,
including recent failures (all are shown, newest first).

The status command supports a `--json` flag for machine-readable output.
It also supports single-item lookup by sequence ID or candidate change ID via
the `--resolve` flag.

Status is intended as a helpful tool for jjq users, not as a comprehensive
history of jjq's operation.

### Check output

jjq writes the check command's combined stdout/stderr to `.jj/jjq-run.log` and
provides `jjq tail` to view it (optionally following updates during a run).

### Deleting queued and failed items.

The jjq command `delete` takes an ID argument and removes the item from the
queue, or for a failed item, its bookmark. If the failed workspace directory is
found, it is removed as well.

### Cleaning up

Runner workspaces are short-lived and are garbage collected upon success.
Failed workspaces persist for debugging and can be cleaned with `jjq clean`
(removes all `jjq-run-*` workspaces it finds).

### Dry-run checking

The `check` command runs the configured check command against a revision in a
temporary workspace, without any queue processing. This lets users verify their
check command is sane before queuing items. The workspace is always cleaned up.

### Diagnostics

The jjq `doctor` command validates the environment: trunk bookmark exists,
check command configured, locks not held, and no orphaned workspaces. Each
check is reported as ok, WARN, or FAIL, with suggested fixes for actionable
issues.

### User configuration

Users may configure jjq via a jjq `config` command. The store of the
configuration data shall be the jj repo, alongside other jjq state like the
sequence ID store and log.

Users may configure:

  - the check command (required — must be set before first run)
  - the name of the trunk bookmark (default "main")
  - the landing strategy: `rebase` (default) or `merge`
  - (status shows all failed items)

### Use of jj bookmarks

jj bookmarks indicate queue and failed items and the metadata branch head:

- `jjq/queue/NNNNNN` — queued items (zero-padded sequence ID)
- `jjq/failed/NNNNNN` — failed merge attempts
- `jjq/_/_` — head of the isolated metadata branch (last_id, config, ops log)

### Using `jj`

jjq uses the `jj` command for all of its operations that interact with the jj
repository. There is no jj API used by jjq, other than that provided by the
output of the `jj` command. To that end, for commands that require querying jj
for information, jjq commands use the templating option that most `jj`
subcommands support, especially `jj log -T'...'`. This allows jjq to control
the "shape" of the output for textual processing and not be broken by small
version-to-version formatting changes in `jj`'s default human-friendly outputs.

### Conforming implementation behaviors

The following behaviors are required for conforming jjq implementations:

  - When an operation modifies the jjq metadata branch (e.g., config changes,
    push, etc.), the implementation must not create empty commits. If a
    workspace operation creates a commit with actual file changes, subsequent
    log operations should edit that existing commit rather than creating a new
    empty child commit.
