# SubmitQueue Ideas for jjq

## Status

Implemented. All four steps were independently reviewed and landed through
jjq's protected-main workflow.

## Goal

Adopt the parts of Uber SubmitQueue that improve a local, jj-native merge
queue without importing its distributed-service architecture:

1. durable lifecycle history;
2. speculative parallel checking of predicted future trunk states; and
3. cooperative cancellation and richer running status.

The queue remains FIFO. A check result is usable only for the exact candidate,
base, result tree, and check command that produced it. Heuristics may influence
which work runs, but never whether a revision is safe to land.

## Non-goals

- MySQL, RPCs, message brokers, service decomposition, or dead-letter queues.
- Probabilistic scoring or enumeration of every success/failure combination.
- Treating file overlap as proof that two changes are independent.
- Priority or preemption policies before jjq has parallel check capacity.
- A generic CI-provider abstraction.

## Step 1: Durable lifecycle history

Add an append-only, jj-native history for each queue submission and expose it
through:

```sh
jjq history <id>
jjq history --change <change-id>
jjq history <id> --json
```

Record the transitions jjq owns: queued, claimed, recovered, check started,
failed, skipped because empty, landed, requeued, replaced, and deleted. Event
kind names form a defined versioned vocabulary rather than arbitrary strings.
Events include an ISO-8601 UTC timestamp and relevant stable facts such as
sequence ID, change/commit IDs, trunk/base commit ID, result change ID, failure
reason, conflict paths, and elapsed check time. Optional facts are omitted
rather than emitted as empty strings.

History lives on `jjq/_/_`, using the existing metadata writer and lock. Reads
pin one metadata snapshot. Queue mutations use a repo-wide `mutation` lock;
the short-lived `publication` lock couples queue bookmark publication with its
`queued` event and runner claims with their `claimed` event. Updating current
queue bookmarks and recording other corresponding events remain separate jj
operations, so history is an audit trail, not the authority for current state.
A crash between them may omit the last event; recovery records what it observes
and current bookmarks remain the source of truth.

The human view is chronological and concise. JSON has a versioned top-level
shape so future event fields can be added without changing the meaning of
existing fields. Change-ID lookup may return multiple submissions, preserving
their separate sequence IDs and retry history.

The failed-to-queued requeue transition is serialized per failed sequence ID.
If a process crashes after creating the replacement queue bookmark but before
removing the failed bookmark, the next requeue reuses that replacement instead
of allocating a duplicate and completes the linked history.

Landing boundary:

- all lifecycle transitions have end-to-end coverage;
- concurrent metadata writers cannot lose events;
- history survives queue/failed bookmark deletion;
- text and JSON output are snapshot-tested;
- `cargo test` passes; and
- the change lands through `jjq push @` followed by `jjq run`.

## Step 2: Separate checking from landing

Refactor a queue attempt into explicit phases:

```text
materialize predicted revision -> check -> validation receipt -> land
```

A successful check produces an in-memory validation receipt containing at
least:

- sequence ID;
- candidate change and commit IDs;
- base/trunk commit ID;
- checked result tree or commit ID;
- landing strategy; and
- a stable fingerprint of the check command.

Landing consumes a receipt only after verifying the queue claim, current trunk,
candidate identity, strategy, and checked tree still match. Step 2 remains
single-worker: it establishes the safety boundary without changing scheduling.

Landing boundary:

- existing behavior and exit codes remain compatible;
- stale or mismatched receipts are rejected and safely requeued;
- both merge and rebase strategies have receipt-focused tests; and
- `cargo test` passes and the change lands through jjq.

## Step 3: Optimistic parallel lookahead

Extend batch execution with an explicit resource cap:

```sh
jjq run --all --jobs N
```

`--jobs 1` is the default and preserves sequential behavior. For queued items
`A`, `B`, and `C` on trunk `T`, a larger value may check these predicted states
in parallel:

```text
T + A
T + A + B
T + A + B + C
```

The first implementation accelerates `merge` strategy. Its predicted merge
commits receive their final description before checking, so an unmodified check
leaves an exact reusable base. A check that snapshots changes invalidates its
dependents. Current `rebase` materialization uses a disposable duplicate with
a different commit identity from the eventual rebased original; it intentionally
falls back to serial execution for `--jobs > 1` until an identity-preserving
materializer exists.

Results still land in FIFO order. When `A` lands, the receipt for `B` becomes
eligible because its predicted base now matches. If `A` fails, receipts that
assumed `A` are invalid and later items are rescheduled from the actual trunk,
excluding the failed item. No result is reused merely because changed files
appear disjoint.

Speculative workspaces remain temporary. Minimal live run state belongs under
the shared `.jj/jjq/` directory so all workspaces report the same state and a
crashed runner can clean it up. Durable outcomes go through Step 1 history.

Landing boundary:

- jobs never exceed the configured cap;
- landing remains FIFO and uses Step 2 receipts;
- an early failure invalidates and reschedules dependent work correctly;
- interruption leaves recoverable claims and no unsafe landing opportunity;
- `status --json` exposes active checks without breaking existing fields;
- race-sensitive and end-to-end tests pass; and
- `cargo test` passes and the change lands through jjq.

## Step 4: Cooperative cancellation and running status

Add:

```sh
jjq cancel <id>
```

Cancellation is intent, not a rollback:

- queued work is removed immediately;
- checking work receives a shared cancellation marker and jjq terminates its
  check process cleanly;
- a landing that has crossed its final compare-and-swap may still win; and
- terminal outcomes take precedence over cancellation intent.

The never-reused sequence ID names a durable marker under
`.jj/jjq/cancellations/`. `cancel` and every runner terminal choice serialize
on `publication.lock`, making the winner deterministic. The runner thread that
owns the check process sends TERM to its process group, escalates to KILL after
one second, and reaps it; `cancel` never signals a recorded PID. A canceled
history event is made durable before the claim and marker are cleaned up, so a
crash resumes cancellation instead of requeuing or rerunning the item.

Expand status with checking/cancelling state, elapsed time, predicted base, and
whether the attempt is speculative. Preserve the existing top-level JSON fields
and add new fields compatibly.

Landing boundary:

- queued and running cancellation have end-to-end tests;
- process termination and cleanup are verified;
- cancellation/landing races have deterministic safe outcomes;
- history records cancellation intent and terminal outcome;
- `cargo test` passes and the change lands through jjq.

Implementation coverage includes queued and in-check cancellation, a completed
speculative result waiting behind FIFO predecessors, TERM-ignoring descendants,
crash recovery after accepted intent, cancellation on both sides of the final
landing CAS boundary, and cancellation with `--stop-on-failure`.

## Possible later work

Only after measuring the optimistic implementation:

- continuous `jjq run --watch` operation;
- smarter selection of which predicted states to check;
- safe test-result caching keyed by exact trees and command fingerprints; or
- file/target overlap as a scheduling hint and status diagnostic.

These are not part of the accepted four-step plan.
