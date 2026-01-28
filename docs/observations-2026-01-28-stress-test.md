# jjq Stress Test Observations - 2026-01-28

Observations from coordinating 4 parallel agents implementing features in separate jj workspaces, processing through jjq merge queue.

## Issues & Proposed Improvements

### 1. Cascading Conflict Resolution

**Problem:** When an item conflicts with trunk, the user resolves it and retries. But if other items merge while the fix is queued, the resolution becomes stale and conflicts again. In testing, the logging middleware required 3 resolution attempts because trunk kept evolving.

**Proposed mitigations:**
- Warn users when retrying against a trunk that has moved since the failure
- Consider auto-rebasing retry candidates onto current trunk before queueing
- Add `jjq retry --rebase` option to explicitly rebase before requeueing

### 2. No Conflict Resolution Guidance

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

### 3. Retry Renumbers Items

**Problem:** When item 2 failed and was retried, it became item 4. The original ID is lost, making it hard to track the lineage of a change through multiple retry attempts.

**Proposed mitigations:**
- Show original ID in status: `4: (retry of 2) lulnzmls resolved: merge...`
- Or keep the original ID and use sub-IDs: `2.1`, `2.2` for retries
- Add a `jjq history <id>` command to show retry chain

### 4. No Relationship View Between Failed and Pending Items

**Problem:** `jjq status` shows pending and failed items separately, but there's no way to see which failed item a pending retry corresponds to, or which pending items might conflict with each other.

**Proposed fix:** Add `jjq status --verbose` or `jjq status -v` showing:
- Retry relationships (item 4 is retry of failed item 2)
- Common ancestors / potential conflict indicators

### 5. Interactive Prompt Blocks Automation

**Problem:** First-time setup asks "Configure jj log to hide jjq metadata?" which blocks automated/scripted workflows. Requires `NON_INTERACTIVE=1` environment variable.

**Proposed mitigations:**
- Auto-detect non-interactive shells (check if stdin is a tty)
- Default to non-interactive behavior, use `--interactive` flag to opt-in
- Or just skip the prompt entirely and document the config in `jjq help`

### 6. Failed Workspaces Accumulate

**Problem:** Each failed merge leaves a temp workspace for debugging. These accumulate and aren't cleaned up. After this stress test, there were multiple `/var/folders/.../tmp.*` directories left behind.

**Proposed mitigations:**
- Add `jjq clean` command to remove old failed workspaces
- Auto-cleanup workspaces older than N hours/days
- Track workspace paths in jjq metadata and clean on `jjq delete <id>`
