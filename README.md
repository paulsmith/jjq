# jjq - merge queue for jj

jjq is a lightweight merge queue tool for [jj](https://martinvonz.github.io/jj/), the Git-compatible VCS.

## What it does

jjq lets you queue revisions for merging to your trunk branch. Each queued item is merged with the current trunk and a configurable check command is run. If the check passes, the trunk bookmark advances. If it fails, the item is marked as failed for you to investigate.

This prevents the "it worked on my branch" problem by ensuring every merge passes checks against the latest trunk.

## Installation

Copy `jjq` to somewhere in your `$PATH`:

```sh
cp jjq ~/.local/bin/
chmod +x ~/.local/bin/jjq
```

Requires: bash, jj

## Usage

### Push a revision to the queue

```sh
jjq push @      # push current revision
jjq push abc    # push revision by change ID
```

### Run the queue

Process the next item in the queue:

```sh
jjq run
```

### Check status

```sh
jjq status
```

### Configure

```sh
jjq config                           # show all config
jjq config check_command "make test" # set check command
jjq config trunk_bookmark main       # set trunk bookmark name
jjq config max_failures 5            # set max failures shown in status
```

### Handle failures

When a merge fails, fix the issue and re-push:

```sh
jj rebase -r mychange -d main  # rebase onto current trunk
# resolve any conflicts
jjq push mychange              # clears old failure, re-queues
```

Push is idempotent: re-pushing the same change ID automatically clears any
previous queue or failed entries for that change.

```sh
jjq delete 3          # remove item 3 from queue/failed
jjq clean             # list failed workspaces
jjq clean 3           # clean workspace for failed item 3
jjq clean all         # clean all failed workspaces
```

### View history

```sh
jjq log               # show recent operations
jjq log 50            # show last 50 operations
```

## How it works

jjq stores its state in your jj repository using bookmarks and an isolated branch:

- Queue items: `jjq/queue/000001`, `jjq/queue/000002`, ...
- Failed items: `jjq/failed/000001`, ...
- Metadata branch: `jjq/_/_` (parented to `root()`)

To hide jjq metadata from `jj log`, add this to `.jj/repo/config.toml`:

```toml
[revsets]
log = "~ ::jjq/_/_"
```

(jjq shows this hint on first interactive use)

## Configuration

| Key | Default | Description |
|-----|---------|-------------|
| `trunk_bookmark` | `main` | Bookmark pointing to your trunk |
| `check_command` | `sh -c 'exit 1'` | Command to run on merge candidates |
| `max_failures` | `3` | Number of recent failures to show in status |

## License

MIT
