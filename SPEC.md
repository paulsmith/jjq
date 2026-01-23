# jjq - merge queue for jj

jjq is a merge queue tool for jj, the Git-compatible VCS. jjq is lightweight,
in that it can be used on a jj repo with little ceremony or distruption and
coexist with other processes. jjq is a jj-native merge queue tool, in that it
uses jj features such as bookmarks and workspaces to operate the merge queue.
jjq uses the jj repo as a data store for its queue items, work logs, and other
metadata, while isolating its data from the user's.

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
revisions are muted.

## Design

jjq operates in an existing jj repository that is storing the versions of a
user's source code. jjq stores metadata about the user's intent to merge
candidate revisions directly in the same jj repo.

### The queue

The jjq merge queue is a set of candidate revisions to be merged to the
repository's trunk (more on that later). A merge is a new commit with two
parents: the current trunk revision (pointed to by a bookmark that defaults to
"main" but is user-configurable), and the candidate revision. The trunk is a
revision in the repo that has passed a check (more on checks later). The queue
is processed in FIFO order.

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

The sequence ID store must be protected by a OS-portable lock.

Running the queue must also take a lock - only one merge can be attempted at a
time. Therefore the jjq run command is a singleton.

It is normal for multiple users to simultaneously enqueue merge candidates, but
only one user may run the jjq merge queue.

The namespaced jj bookmark `jjq/lock/run` is used as a global lock that a OS
runner process (the `run` command in jjq CLI) must take. The queue runner
process will attempt to create this bookmark as means of taking the lock. If it
succeeds, meaning the bookmark did not already exist, it succeeds and can
proceed with the merge attempt. If it fails to create the bookmark, that means
it already exists, and an existing runner process holds the lock (possibly
already exited and failed to release the lock by deleting the bookmark).

Similarly, the bookmark `jjq/lock/id` is used to protect concurrent access to
the sequence ID store.

### Merge-to-be

The "merge-to-be" in jjq is the name for the revision that is parented by both
the current trunk and the candidate revision the user queued for merge. It
doesn't exist until a jjq run is executed.

### Conflicts

Producing the merge-to-be from two parents, the trunk and the candidate
revision for merging, can lead to conflicts.

Conflicts are a first-class concept in jj, in that they are stored as objects
in the repo, and don't prevent subsequent operations.

A conflict in a merge-to-be will mark that queue item as having failed.

### Checks

A check in jjq is the command to be run on the commit that is the combination
of the trunk and the candidate revision (i.e., the commit where those are the
two parents).

The check can be any command that can be executed by a POSIX shell. Its exit
code, zero (success) or non-zero (failure), is determinative of the success of
the check.

### Workspaces

jjq uses a jj workspace during a run for the working copy to produce the
merge-to-be and to execute the check on same.

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

When a merge attempt from a queue run fails, either due to a conflict when
creating the merge-to-be, or a failed check, that queue item is marked as
"failed".

The user must then take action to resolve the failure, and either delete it
from the queue, or retry it.

### Retries

Retrying a queue item after it has failed is supported by the jjq CLI. The user,
instead of pushing a revision, retries the sequence ID of the item.

Retries receive a new sequence ID.

Retries never happen automatically. The jjq tool will never retry an item on its
own without a user's explicit intent.

Retries default to pushing the revision pointed to by the failure bookmark. They
can optionally take an explicit revset argument to push that revision instead
and maintain the association with the original queued item.

### Status

jjq users can get a list of the current queue via the CLI's status command,
including recent failures, 3 by default but configurable by the user.

Successful merges and older failures are not included by default in the status
command.

Status is intended as a helpful tool for jjq users, not as a comprehensive
history of jjq's operation. For a history, users should explore jjq's log
command.

### The log

jjq records all its actions in a log. The log is stored in the jj repo
alongside the sequence ID store. The log is meant to capture the sequence of
logical actions jjq takes as it executes its commands, as well as to record
notable output and summaries for future analysis and debugging, especially of
failures. For example, the output of the check command shall be recorded in the
log.

The log should help users reconstruct jjq operational histories, supporting
additional tooling like timeline visualization.

The storage format of the jjq log is left as a detail to conforming
implementations.

### Deleting queued and failed items.

The jjq command `delete` take an ID argument and removes the item from the
queue, or for a failed item, its bookmark.

### Cleaning up

Runner workspaces are short-lived and shall be garbage collected upon success.
Failed workspaces shall persist, to permit user debugging, and can be manually
cleaned up by the user with the jjq `clean` command.

### User configuration

Users may configure jjq via a jjq `config` command. The store of the
configuration data shall be the jj repo, alongside other jjq state like the
sequence ID store and log.

Users may configure:

  - the check command (default is "sh -c 'exit 1'", to encourage a new user to configure it)
  - the name of the trunk bookmark (default "main")
  - the number of most recent failed merges to display via the status command (default 3)

### Use of jj bookmarks

jj bookmarks are the way that jjq indicates what items are in the queue, what
have failed, where the latest metadata (sequence ID, user config) lives, and the
working copy of the runner workspace.

jj bookmarks are also used to gain a global lock for running the jjq merge
queue. Only one OS process may be trying to process the queue at a time. The
existence of a well-known stable jj bookmark indicates the fact that a lock is
taken.

jjq must use the following conventions for its jj bookmarks:

  - All jjq bookmarks are "namespaced" starting with `jjq`, followed by a scope,
    then details on that scope, all delimited with the solidus `/`. This looks
    like the pattern `jjq/SCOPE/DETAILS`.
  - Valid scopes are:
    - `queue` - used for queue items. The zero-padded sequence ID is the
      details field. eg., `jjq/queue/000001`.
    - `failed` - used for failed merge attempts. The zero-padded sequence ID is
      the details field. eg., `jjq/failed/000001`.
    - `run` - used for merge-to-be workspace. The zero-padded sequence ID is
      the details field. eg., `jjq/run/000001`.
    - `lock` - used for mutex-style locking. The type of lock is the details
      field. eg., `jjq/lock/run`.
  - A special bookmark `jjq/_/_` is used to point to the latest revision of its
    isolated (i.e., parented to 'root()') branch of metadata stores.

The `jjq/XXX/XXX` pattern must be adhered to - since Git is the practically
speaking only backing store for jj, and jj exports its bookmarks to the
underlying Git as branches, we can't have bookmark names that look like
"parents" to others in a filesystem-like way. This explains why `jjq/_/_` looks
the way it does.

### Using `jj`

jjq uses the `jj` command for all of its operations that interact with the jj
repository. There is no jj API used by jjq, other than that provided by the
output of the `jj` command. To that end, for commands that require querying jj
for information, jjq commands use the templating option that most `jj`
subcommands support, especially `jj log -T'...'`. This allows jjq to control
the "shape" of the output for textual processing and not be broken by small
version-to-version formatting changes in `jj`'s default human-friendly outputs.
