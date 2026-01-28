# jj Transaction Model and Concurrency

This document describes jj's transaction and concurrency model as it relates to
jjq's design. Understanding these semantics is critical for reasoning about
concurrent access safety.

## Overview

jj uses an **optimistic concurrency model**. Operations are not locked during
execution; instead, concurrent operations may create divergent operation heads
that are automatically reconciled on the next repository load.

## Transaction Lifecycle

A jj transaction proceeds through these phases:

1. **Load**: Read the repository at the current operation head
2. **Mutate**: Make changes in memory to a `MutableRepo`
3. **Write**: Persist the new view and operation to storage
4. **Publish**: Atomically update operation heads (under lock)

```
┌─────────────────────────────────────────────────────────────┐
│                        Transaction                          │
├─────────┬─────────┬─────────┬───────────────────────────────┤
│  Load   │ Mutate  │  Write  │           Publish             │
│         │         │         │  ┌───────────────────────┐    │
│         │         │         │  │ acquire op_heads lock │    │
│         │         │         │  │ update op_heads       │    │
│         │         │         │  │ release lock          │    │
│         │         │         │  └───────────────────────┘    │
└─────────┴─────────┴─────────┴───────────────────────────────┘
     ▲                                      ▲
     │                                      │
  No lock                            File lock held
  held here                          (flock on Unix)
```

## The Publish Lock

The only lock jj acquires is during the publish phase. From
`simple_op_heads_store.rs`:

```rust
async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
    let lock = FileLock::lock(self.dir.join("lock"))?;
    Ok(Box::new(SimpleOpHeadsStoreLock { _lock: lock }))
}
```

This lock serializes updates to operation heads but does NOT prevent concurrent
transactions from proceeding through the load/mutate/write phases.

## Divergent Operations

When two processes run jj commands concurrently without seeing each other's
changes, both will publish successfully, creating **divergent operation heads**.

From jj source comments:

> "It's fine if the old head was not found. It probably means that we're on a
> distributed file system where the locking doesn't work. We'll probably end up
> with two current heads. We'll detect that next time we load the view."

### Automatic Reconciliation

On the next repository load, jj detects multiple operation heads and
automatically merges them:

1. Acquire the op_heads lock
2. Re-check if still divergent (another process may have reconciled)
3. If still divergent, create a merge operation
4. Update op_heads to the merged operation

## Bookmark Merge Semantics

When reconciling divergent operations that modified bookmarks, jj uses
three-way merging with `SameChange::Accept` semantics:

| Base | Left | Right | Result |
|------|------|-------|--------|
| absent | X | X | X (trivially resolved) |
| absent | X | Y | **conflicted** |
| X | Y | Y | Y (same change accepted) |
| X | Y | Z | **conflicted** |

A conflicted bookmark stores multiple targets as a `Merge<Option<CommitId>>`.

## Implications for `jj bookmark create`

The `jj bookmark create` command:

1. Loads the repository
2. Checks if the bookmark already exists (returns error if so)
3. Starts a transaction
4. Creates the bookmark in the mutable view
5. Commits the transaction

**Critical**: The existence check happens at step 2, before the transaction.
Two concurrent `bookmark create` commands can both pass the check, then both
succeed in creating the bookmark.

### Race Window

```
Process A                          Process B
─────────                          ─────────
Load repo (no bookmark)
                                   Load repo (no bookmark)
Check: absent ✓
                                   Check: absent ✓
Start transaction
                                   Start transaction
Create bookmark → X
                                   Create bookmark → X
Publish (gets lock first)
                                   Publish (gets lock second)
Exit success                       Exit success
                        ↓
              Both think they "created" the bookmark
              Divergent ops auto-reconciled on next load
```

If both point to the same target, the merge resolves trivially. If different
targets, the bookmark becomes conflicted.

## Why This Matters for jjq

jjq uses `jj bookmark create` as a mutex lock:

```bash
if ! jj bookmark create -r "$jjq_bookmark" "$lock" >/dev/null 2>&1; then
    # Lock held by another process
    return 1
fi
```

This is **not truly safe** for concurrent access because:

1. Two processes can race past the existence check
2. Both will "successfully" create the lock bookmark
3. The divergent operations will be silently reconciled
4. Both processes proceed, believing they hold the lock exclusively

## Observed Race Window

The race window is the time between:
- Process A loading the repo (step 1)
- Process A publishing the transaction (step 5)

In practice, this is typically sub-second. However, it is non-zero and can be
extended if the system is under load or I/O is slow.

## Conclusion

jj's optimistic concurrency model is designed for:
- Single-user workflows
- Distributed filesystems where locking is unreliable
- Eventual consistency with automatic conflict detection

It is NOT designed to provide mutual exclusion guarantees. Any application
requiring true mutex semantics must implement external locking.

---

## jjq's Solution: mkdir-based Locking

jjq uses `mkdir(2)` atomicity for true mutual exclusion. The `mkdir` system
call is atomic: it either succeeds (directory created, lock acquired) or fails
(directory exists, lock held by another process). There is no race window.

### Implementation

Lock files are stored in `.jj/jjq-locks/` within the repository:

```
.jj/jjq-locks/
├── id/          # Sequence ID lock (held during push/retry)
│   └── pid      # PID of lock holder
└── run/         # Run lock (held during queue processing)
    └── pid      # PID of lock holder
```

### Lock Acquisition

```bash
acquire_lock() {
    local name="$1"
    local lockdir="${JJQ_LOCK_DIR}/${name}"

    if mkdir "$lockdir" 2>/dev/null; then
        echo $$ > "$lockdir/pid"
        return 0
    fi
    return 1
}
```

The `mkdir` succeeds atomically or fails—no TOCTOU race possible.

### Advantages Over Bookmark-based Locking

| Aspect | Bookmark Lock | mkdir Lock |
|--------|---------------|------------|
| Atomicity | Race window exists | Truly atomic |
| Portability | Requires jj | POSIX standard |
| Visibility | `jj log` shows lock | `ls .jj/jjq-locks/` |
| Stale detection | Manual bookmark delete | PID file enables detection |
| Complexity | jj transaction overhead | Simple syscall |

### Stale Lock Recovery

If a process crashes while holding a lock, the lock directory remains. Users
can detect stale locks by checking if the PID in the lock directory is still
running:

```bash
# Check if lock holder is still alive
if [ -f .jj/jjq-locks/run/pid ]; then
    pid=$(cat .jj/jjq-locks/run/pid)
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "Stale lock from dead process $pid"
        rm -rf .jj/jjq-locks/run
    fi
fi
```

This is left as a manual operation; automatic stale lock detection risks
incorrectly breaking locks on systems where PIDs wrap around quickly.
