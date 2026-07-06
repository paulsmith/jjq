# jjq - merge queue for jj

jjq is a lightweight, local merge queue tool for
[jj](https://martinvonz.github.io/jj/) (Jujutsu), the Git-compatible VCS.

## What it does

jjq lets you queue revisions for merging to your trunk branch (eg.,
`main` bookmark). Each queued item is merged with the current trunk and a
configurable check command is run. If the check passes, the trunk bookmark
advances. If it fails, or there were conflicts with the up-to-date trunk,
the item is marked as failed for you to investigate.

This prevents the "it worked on my branch" problem by ensuring every merge
passes checks against the latest trunk.

## Installation

Prerequisite: make sure `jj` is installed.

### Homebrew

```sh
brew install paulsmith/tap/jjq
```

### Download a release

Download the tarball for your platform from the
[latest release](https://github.com/paulsmith/jjq/releases/latest) and run
the included install script:

```sh
curl -LO https://github.com/paulsmith/jjq/releases/latest/download/jjq-VERSION-PLATFORM.tar.gz
tar xzf jjq-VERSION-PLATFORM.tar.gz
cd jjq-VERSION-PLATFORM
sudo ./install                    # installs to /usr/local
```

Available platforms: `aarch64-darwin`, `x86_64-darwin`, `aarch64-linux`,
`x86_64-linux`.

To install to a different prefix (no sudo needed):

```sh
PREFIX=$HOME/.local ./install
```

### Build from source

Requires Rust (edition 2024) and Cargo, or Nix.

With Cargo:

```sh
git clone https://github.com/paulsmith/jjq.git
cd jjq
cargo build --release
install -m 755 target/release/jjq /usr/local/bin/
```

With Nix:

```sh
nix build github:paulsmith/jjq
./result/bin/jjq --help
```

## Usage

### Initialize

Set up jjq in your repository:

```sh
jjq init
```

Or non-interactively:

```sh
jjq init --trunk main --check "make test"
```

### Push a revision to the queue

Any revset will do so long as it resolves to a single revision.

```sh
jjq push @      # push current revision
jjq push abc    # push revision by change ID
```

Common push choices:

```sh
jjq push @       # queue the working-copy commit
jjq push @-      # queue the parent of an empty working-copy commit
jjq push feature # queue the tip of a named workspace or bookmark
```

For a stack of related revisions, push the tip revision as one queue item.
`jjq push` accepts a revset that resolves to one revision, so do not push a
range like `base::tip`. If `feature` points at the final meaningful revision in
a stack, `jjq push feature` queues the whole branch of changes ending there.

With the default `rebase` strategy, jjq tests the branch from trunk to that tip
and then rebases the branch onto the latest trunk before moving the trunk
bookmark. With the `merge` strategy, jjq creates a merge commit with the current
trunk and the queued tip as parents.

If your current `@` is only an empty parking commit above the finished stack,
prefer `jjq push @-` or a bookmark/workspace name that points at the stack tip.
Pushing the empty `@` can still include the ancestor changes, but the queued
revision itself is the empty tip.

### Run the queue

Process the next item in the queue:

```sh
jjq run
```

Drain the entire queue (continues past failures by default):

```sh
jjq run --all
```

Stop at the first failure instead:

```sh
jjq run --all --stop-on-failure
```

### Check status

```sh
jjq status                          # overview of queue and recent failures
jjq status --json                   # machine-readable JSON output
jjq status 42                       # detail view of item 42
jjq status 42 --json                # detail view as JSON
jjq status --resolve <change_id>    # look up item by candidate change ID
```

### Configure

After initialization, change settings with:

```sh
jjq config                           # show all config
jjq config check_command "make test" # set check command
jjq config trunk_bookmark main       # set trunk bookmark name
```

### Handle failures

When a merge fails, the simplest option is `requeue`:

```sh
jj rebase -b mychange -d main  # rebase onto current trunk
# resolve any conflicts
jjq requeue 3                  # re-push failed item 3 (runs conflict check first)
```

Or push the fixed revision directly:

```sh
jjq push mychange              # clears old failure, re-queues
```

Push is idempotent: re-pushing the same change ID automatically clears any
previous queue or failed entries for that change. Re-pushing the exact same
commit ID that is already queued is rejected as a duplicate.

```sh
jjq delete 3          # remove item 3 from queue/failed
jjq clean             # remove all orphaned jjq workspaces
```

### Test your check command

```sh
jjq check              # run check against current working copy
jjq check --rev main   # run check against a specific revision
jjq check -v           # show workspace path, shell, and env vars
```

View recent check output (tail the log):

```sh
jjq tail               # last 20 lines; follows by default
jjq tail --all         # from the beginning
jjq tail --no-follow   # dump once and exit
```

### Validate your setup

```sh
jjq doctor
```

Checks the trunk bookmark, check command, sequence counter, conflicted jjq
bookmarks, lock state, stale claims, and workspace preconditions. Catches
common config errors before queue items fail. `jjq doctor --fix` repairs a
conflicted jjq metadata bookmark.

## How it works

jjq stores its state in your jj repository using bookmarks and an isolated
branch:

- Queue items: `jjq/queue/000001`, `jjq/queue/000002`, ...
- Running item: `jjq/running/000001` (claimed by an active queue runner)
- Failed items: `jjq/failed/000001`, ...
- Metadata branch: `jjq/_/_` (parented to `root()`)

`jjq init` automatically configures `jj log` to hide jjq metadata.
For repositories initialized before this feature, run:

```sh
jj config set --repo revsets.log '~ ::bookmarks(exact:"jjq/_/_")'
```

## Configuration

| Key                | Default              | Description                                                      |
|--------------------|----------------------|------------------------------------------------------------------|
| `trunk_bookmark`   | `main`               | Bookmark pointing to your trunk                                  |
| `check_command`    | *(set during init)*  | Command to run on merge candidates (required before running)     |
| `strategy`         | `rebase`             | Strategy for landing the candidate on trunk (`rebase` or `merge`). Existing repos without this key default to `merge` for backward compatibility. |

## Copying

[BSD](./COPYING)
