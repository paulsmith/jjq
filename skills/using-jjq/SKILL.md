---
name: using-jjq
description: Use when landing, merging, or pushing changes to trunk (main) in a jj (Jujutsu) repository that uses jjq — signs include .jj/jjq/ existing, jjq/queue or jjq/failed bookmarks, jjq mentioned in project docs, or jjq on PATH. Also use when a change needs to land on main, when a jjq push or run reports a conflict or failed check, or when a queue item needs retrying, deleting, or debugging.
---

# Using jjq (merge queue for jj)

## Overview

jjq is a local merge queue for jj: it lands revisions on the trunk bookmark
one at a time, gating each on the project's check command. In a jjq repo,
**the queue is the only way trunk moves.**

Never move trunk yourself — no `jj bookmark set/move` on trunk, no `jj tug`,
no `git` pushes or merges to trunk. Landing without jjq skips the checks the
queue exists to enforce. If you already moved trunk by hand: find the
operation in `jj op log`, undo it with `jj op revert <op-id>`, then land
properly.

## Landing a change

1. `jjq status` — see the queue, failures, and whether a runner is active.
2. `jjq push <revset>` — e.g. `jjq push @`. Push is cheap: it pre-checks the
   merge and refuses immediately (exit 1) with recovery instructions if it
   would conflict. Pushing your working copy (`@`) also parks it on a new
   empty child ("working copy moved to a new change; the queued commit stays
   as pushed"), so later edits can't leak into the landed commit — expect an
   empty `@` afterwards and just keep working there.
3. Nothing processes the queue automatically. If project instructions name a
   dedicated queue runner (person or process), stop after pushing. Otherwise
   run it yourself: `jjq run` (next item) or `jjq run --all` (drain).
4. Verify: `jjq status` shows your change under "Landed (recent)".

## When a push or run fails

jjq prints the exact recovery commands with your real change IDs — follow
them. The shape is always:

```sh
jjq status <id>                   # failure detail: reason, conflicting files
jj rebase -r <your-change> -d main   # rebase onto current trunk
# edit files to resolve conflict markers
jjq check --rev <your-change>     # optional: pre-verify the fix
jjq push <your-change>            # re-push; clears the failed entry itself
```

`jjq requeue <id>` retries a failed item *unchanged*; after editing, use
`jjq push`. `jjq delete <id>` drops an item. `jjq tail --no-follow` shows
the check output of the last run.

## Rules that keep sessions unstuck

| Situation | Do this |
|---|---|
| Viewing check output | `jjq tail --no-follow` — bare `jjq tail` follows the log and hangs a non-interactive session |
| First-time setup | `jjq init --trunk main --check '<cmd>'` — without flags it prompts interactively |
| "another queue runner is active" (exit 1) | Don't retry in a loop — check `jjq status`; the other runner is landing items |
| "working copy is stale" after a landing | `jj workspace update-stale` in that workspace |
| Parallel agents/developers | One jj workspace each (`jj workspace add`); push from each; the queue serializes landings |
| Anything looks broken | `jjq doctor` (and `jjq doctor --fix` for a conflicted metadata bookmark) |

Exit codes: 0 ok · 1 conflict / check failed / runner active · 2 batch
partially failed · 3 metadata lock timeout · 10 usage error.
`jjq status --json` gives machine-readable state. `jjq quickstart` prints
the tool's built-in agent guide; `man jjq` has full docs.

## Red flags — stop and use the queue

- About to run `jj bookmark set/move` or `jj tug` on trunk
- About to `git push`, `git merge`, or `git commit` to trunk
- "The queue seems broken, I'll just move the bookmark this once" — run
  `jjq doctor` instead
- "My change is trivial, it doesn't need the check" — trivial changes fail
  checks too; push it

All of these mean: `jjq push`, then `jjq run`.
