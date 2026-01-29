# jjq Stress Test Observations - 2026-01-28

## Run 1

Observations from coordinating 4 parallel agents implementing features in separate jj workspaces, processing through jjq merge queue.

### Issues & Proposed Improvements

#### 1. Cascading Conflict Resolution

**Problem:** When an item conflicts with trunk, the user resolves it and retries. But if other items merge while the fix is queued, the resolution becomes stale and conflicts again. In testing, the logging middleware required 3 resolution attempts because trunk kept evolving.

**Proposed mitigations:**
- Warn users when retrying against a trunk that has moved since the failure
- Consider auto-rebasing retry candidates onto current trunk before queueing
- Add `jjq retry --rebase` option to explicitly rebase before requeueing

#### 2. No Conflict Resolution Guidance

**Problem:** When a merge fails, jjq outputs:
```
jjq: merge 000002 has conflicts, marked as failed
jjq: workspace remains: /var/folders/.../tmp.abc123
```

Users unfamiliar with the workflow don't know what to do next.

**Proposed fix:** Add actionable guidance:
```
jjq: merge 000002 has conflicts, marked as failed
jjq: workspace remains: /var/folders/.../tmp.abc123
jjq:
jjq: To resolve:
jjq:   1. Fix conflicts in the workspace above (or create a new revision)
jjq:   2. Run: jjq retry 2 <fixed-revset>
```

#### 3. Retry Renumbers Items

**Problem:** When item 2 failed and was retried, it became item 4. The original ID is lost, making it hard to track the lineage of a change through multiple retry attempts.

**Proposed mitigations:**
- Show original ID in status: `4: (retry of 2) lulnzmls resolved: merge...`
- Or keep the original ID and use sub-IDs: `2.1`, `2.2` for retries
- Add a `jjq history <id>` command to show retry chain

#### 4. No Relationship View Between Failed and Pending Items

**Problem:** `jjq status` shows pending and failed items separately, but there's no way to see which failed item a pending retry corresponds to, or which pending items might conflict with each other.

**Proposed fix:** Add `jjq status --verbose` or `jjq status -v` showing:
- Retry relationships (item 4 is retry of failed item 2)
- Common ancestors / potential conflict indicators

#### 5. Interactive Prompt Blocks Automation

**Problem:** First-time setup asks "Configure jj log to hide jjq metadata?" which blocks automated/scripted workflows. Requires `NON_INTERACTIVE=1` environment variable.

**Proposed mitigations:**
- Auto-detect non-interactive shells (check if stdin is a tty)
- Default to non-interactive behavior, use `--interactive` flag to opt-in
- Or just skip the prompt entirely and document the config in `jjq help`

#### 6. Failed Workspaces Accumulate

**Problem:** Each failed merge leaves a temp workspace for debugging. These accumulate and aren't cleaned up. After this stress test, there were multiple `/var/folders/.../tmp.*` directories left behind.

**Proposed mitigations:**
- Add `jjq clean` command to remove old failed workspaces
- Auto-cleanup workspaces older than N hours/days
- Track workspace paths in jjq metadata and clean on `jjq delete <id>`

---

## Run 2

Second stress test run with 3 parallel agents implementing features (goodbye, stats, time), followed by a sneaky bug introduction and bugfix. This run focused on observing improvements since Run 1 and finding remaining UX/correctness issues.

### What Worked Well

1. **Conflict resolution guidance is now present.** jjq now outputs clear instructions after a conflict:
   ```
   jjq: To resolve:
     1. Fix conflicts in the workspace above (or create a new revision)
     2. Run: jjq retry 2 <fixed-revset>
   ```
   This directly addresses Run 1 observation #2.

2. **Retry annotations in status work.** `jjq status` now shows `(retry of N)` next to retried items:
   ```
   4: ssyxtmxzmltl Add /time endpoint (retry of 3)
   ```
   This addresses Run 1 observations #3 and #4.

3. **`jjq clean` works.** Successfully cleaned up 3 orphaned workspaces. Addresses Run 1 observation #6.

4. **`run --all` resilience.** Continues past failures and processes the entire queue, reporting summary counts. Exit code reflects the first failure type.

5. **Pre-flight conflict check on push.** Agents can push without worrying about conflicts blocking other items - the check catches them early.

6. **Agent workflow was smooth.** The push → run → conflict → rebase → retry cycle is understandable and predictable. Agents had no difficulty with the tool semantics.

7. **Workspace isolation works well.** Separate jj workspaces in /tmp prevent agents from stepping on each other.

### Issues Found

#### 7. Cascading Conflicts Still Require Multiple Manual Rounds

**Status:** Still open from Run 1 observation #1.

The stats feature required **3 resolution cycles** in this run:
- Push (queued at 2) → conflict with goodbye → resolved, retried as 5
- Run → conflict with time (which merged while stats was being resolved) → resolved, retried as 6
- Run → merged successfully

Each resolution required a full rebase + manual conflict resolution + test verification + retry. This is the biggest UX pain point for a merge queue tool.

**Root cause:** `jjq retry` does not check whether the revision being retried is based on the current trunk. The user resolves against the trunk they see, but trunk can move before the retry is processed.

**Strongest mitigation:** `jjq retry` should automatically rebase the provided revset onto current trunk before queuing, or at minimum warn if the revset's parent is behind trunk.

#### 8. Duplicate Entries in Failed Status Display

**Possible bug:** One agent observed duplicate entries in `jjq status` output:
```
jjq: Failed (recent):
  2: otvrnqsukmwr Failed: merge 000002 (conflicts)
  2: otvrnqsukmwr Failed: merge 000002 (conflicts)
```

Item 2 appeared twice in the failed section. This may be because item 2 had two failed merge attempts (original 000002 and the merge-to-be from 000005), both referencing the same change ID. The display logic may be counting bookmarks rather than unique items.

**Proposed fix:** Deduplicate failed items by change ID or item ID in the status display.

#### 9. `jj edit @-` Fails on Merge Commits

**Not a jjq bug** but a workflow friction: after `jj edit main` to inspect a merge commit, `jj edit @-` fails with "resolved to more than one revision" because merge commits have multiple parents. This is standard jj behavior but came up during queue management.

#### 10. No Way to See What's Currently on Main Without Editing

**Minor friction:** The queue manager needed to check what `main` looks like after merges. There's no `jjq show main` or similar. Had to use `jj edit main` (which switches working copy) or `jj show main` (which exists in jj itself).

**Not actionable for jjq** - this is a jj workflow concern, not a jjq concern.

### Metrics

| Metric | Value |
|--------|-------|
| Features implemented | 3 (goodbye, time, stats) |
| Total queue items processed | 7 (3 original + 3 retries + 1 bugfix) |
| Merge conflicts encountered | 4 (stats×2, time×1, stats-round-2×1) |
| Conflict resolution cycles | 3 (2 parallel + 1 sequential) |
| Bug introduced and fixed | 1 (wrong JSON key in goodbye endpoint) |
| Final queue items merged | 4 (goodbye, time, stats, bugfix) |
| Failed workspaces cleaned | 3 |

### Summary

jjq has improved substantially since Run 1. The conflict resolution guidance, retry annotations, `clean` command, and `run --all` resilience are all working well. The primary remaining pain point is **cascading conflict resolution** (observation #1/#7) - when trunk moves during resolution, users must re-resolve. An auto-rebase on retry would eliminate most of the friction observed in this test.
