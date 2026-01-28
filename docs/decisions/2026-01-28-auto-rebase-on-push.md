# Decision: Auto-Rebase on Push

**Date:** 2026-01-28
**Status:** Deferred
**Context:** jjq merge queue design

## Summary

Decided to **not** implement automatic rebasing when `jjq push` detects conflicts with trunk. The current behavior (fail with helpful message) is sufficient. Will revisit if real-world usage reveals this as a pain point.

## Background

After adding a pre-flight conflict check to `jjq push`, we considered whether push should automatically rebase the candidate revision onto trunk when conflicts are detected.

### Options Considered

**A. Non-interactive auto-rebase**
- Automatically rebase onto trunk if conflicts detected
- If rebase is clean: proceed with push
- If rebase has conflicts: fail

**B. Flag-based (`--rebase`)**
- Default: fail on conflicts
- With `--rebase`: attempt auto-rebase

**C. Interactive prompt**
- Prompt user: "Rebase onto trunk? [y/N]"

**D. Punt for now** ← Chosen

## Decision

We chose Option D: keep the current "fail on conflicts" behavior and gather real-world usage data before adding complexity.

## Rationale

### Why other merge queue tools don't auto-rebase

Research into GitHub Merge Queue, GitLab Merge Trains, Bors-ng, Mergify, and Zuul showed that **none of them auto-rebase**. All require manual conflict resolution. Reasons:

1. They operate remotely - user not present to consent to changes
2. Rebasing changes commit identity, breaking PR/review associations
3. Conflict resolution requires human judgment
4. Even clean rebases can introduce test failures against new trunk

### jj-specific considerations

jj's model is different from git:
- Rebases always "succeed" - conflicts become first-class objects stored in the revision
- Change IDs are stable across rebases (unlike git commit SHAs)
- The question isn't "will rebase succeed" but "will the rebased revision have conflicts"

### Risks of auto-rebase

1. **Silent revision mutation** - User pushes revision `abc`, but jjq queues `xyz` (the rebased version). This is surprising behavior.

2. **Multiple revisions exist** - Both original and rebased revisions exist. User's working copy may still be on the original.

3. **Non-idempotent** - Running `jjq push abc` twice could produce different results if trunk moved between invocations.

4. **Ambiguous ownership** - The rebased revision was created by jjq, not the user. Who "owns" it?

### Why the current UX is sufficient

The error message tells the user exactly what to do:

```
jjq: revision 'pr1' conflicts with main
jjq: rebase onto main and resolve conflicts before pushing
```

The manual workflow is composable and explicit:

```bash
jj rebase -r pr1 -d main && jjq push pr1
```

Since jjq is a **local** tool (not a CI service), the user is present and can immediately act on failures. The friction from "fail fast and tell me what to do" is minimal.

## Future Path

If real-world usage reveals that manual rebasing is a significant friction point, we would implement Option B (`--rebase` flag) with these semantics:

1. Check if revision conflicts with trunk
2. If conflicts, run `jj rebase -r $revset -d $trunk_bookmark`
3. Check if rebased revision has jj conflicts (first-class conflicts)
4. If still has conflicts: fail with "Rebased but conflicts remain"
5. If no conflicts: queue the rebased revision with clear output

We would NOT:
- Auto-update the user's working copy
- Delete the original revision
- Make `--rebase` the default behavior

## References

- Testing report: `docs/testing/2026-01-27-parallel-agent-testing.md`
- Research on other merge queues: Agent research task (2026-01-28)
