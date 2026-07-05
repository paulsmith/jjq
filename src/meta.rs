// ABOUTME: Single-writer discipline for the jjq metadata branch (jjq/_/_).
// ABOUTME: All metadata writes flow through with_meta_workspace under one lock.

// Lock-order invariant: a process may acquire the `meta` lock while holding
// the `run` lock (run_one records workspace metadata mid-run), but no code
// path may acquire the `run` lock while holding `meta`. Keeping the order
// run -> meta acyclic makes deadlock impossible.

use anyhow::{Result, bail};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::lock::Lock;

/// The jjq metadata bookmark name.
pub const JJQ_BOOKMARK: &str = "jjq/_/_";

/// Revset pinning the metadata bookmark by exact name. A conflicted bookmark
/// makes this resolve to multiple revisions, which callers treat as a hard
/// error rather than silently reading one side of the conflict.
pub fn meta_revset() -> String {
    format!("bookmarks(exact:\"{}\")", JJQ_BOOKMARK)
}

/// Default time to wait for the metadata lock before giving up.
const LOCK_TIMEOUT: Duration = Duration::from_millis(15_000);

/// How often to retry the lock while waiting.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Process-local counter distinguishing metadata workspaces created by the
/// same process (e.g. a push allocates an ID, then records that the log hint
/// was shown).
static WORKSPACE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Upper bound on the configurable lock timeout; also keeps the deadline
/// arithmetic overflow-free.
const LOCK_TIMEOUT_CAP: Duration = Duration::from_millis(3_600_000);

fn lock_timeout() -> Duration {
    match std::env::var("JJQ_META_LOCK_TIMEOUT_MS") {
        Ok(v) => match v.parse::<u64>() {
            Ok(ms) => Duration::from_millis(ms).min(LOCK_TIMEOUT_CAP),
            Err(_) => {
                eprintln!(
                    "jjq: warning: invalid JJQ_META_LOCK_TIMEOUT_MS '{}'; using default",
                    v
                );
                LOCK_TIMEOUT
            }
        },
        Err(_) => LOCK_TIMEOUT,
    }
}

/// Acquire the metadata lock, waiting up to the timeout for a concurrent
/// writer to finish. Metadata writes are short (a few jj calls), so waiting
/// briefly beats failing a push that raced a config write.
fn acquire_lock() -> Result<Lock> {
    let timeout = lock_timeout();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(lock) = Lock::acquire("meta")? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(ExitError::new(
                exit_codes::LOCK_HELD,
                format!(
                    "could not acquire metadata lock within {:?} (another jjq process may be stuck; see 'jjq doctor')",
                    timeout
                ),
            )
            .into());
        }
        thread::sleep(LOCK_POLL_INTERVAL);
    }
}

/// Forgets the temporary metadata workspace when dropped, so error paths
/// cannot leak workspaces.
struct WorkspaceGuard {
    name: String,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if let Err(e) = jj::workspace_forget(&self.name) {
            // The guard is armed before `workspace add`, so when the add
            // itself failed there is legitimately nothing to forget.
            if !e.to_string().contains("No such workspace") {
                eprintln!(
                    "jjq: warning: failed to forget workspace {}: {}",
                    self.name, e
                );
            }
        }
    }
}

/// Run `mutate` in a temporary workspace on the metadata branch and commit
/// the result, serialized under the repo-scoped `meta` lock. This is the only
/// way jjq writes to the metadata branch: one lock, one writer, one bookmark
/// move at a time.
///
/// `mutate` receives the workspace root, writes files via absolute paths, and
/// returns the commit description recorded on the metadata branch (the
/// branch's commit messages double as an operation log). The branch — with
/// its `last_id` seed — is created under the lock on first use. The temporary
/// workspace is always forgotten, on success or error, and the process
/// working directory is never changed.
pub fn with_meta_workspace(mutate: impl FnOnce(&Path) -> Result<String>) -> Result<()> {
    let _lock = acquire_lock()?;

    // Create the metadata branch under the lock on first use.
    let created = if jj::bookmark_exists(JJQ_BOOKMARK)? {
        false
    } else {
        let change_id = jj::new_rev(&["root()"])?;
        jj::describe(&change_id, "jjq metadata root")?;
        jj::run_quiet(&["bookmark", "create", "-r", &change_id, JJQ_BOOKMARK])?;
        true
    };

    // Pin the bookmark to a single revision before adding a workspace; a
    // conflicted bookmark resolves to several and is unusable as a parent.
    if let Err(e) = jj::resolve_revset_full(&meta_revset()) {
        if e.to_string().contains("multiple revisions") {
            bail!(
                "jjq metadata bookmark ({}) is conflicted — run 'jjq doctor'",
                JJQ_BOOKMARK
            );
        }
        return Err(e);
    }

    // Seed the sequence counter when it is absent: always for a branch
    // created above, and as self-healing for a branch whose creator crashed
    // between `bookmark create` and its first `bookmark set` (bookmark exists
    // but points at the bare root commit). A branch with real history that
    // lost last_id is corruption and must not be silently reseeded.
    let needs_seed = if created {
        true
    } else {
        match jj::file_show("last_id", &meta_revset()) {
            Ok(_) => false,
            Err(e) if e.to_string().contains("No such path") => {
                let parent_is_root = jj::run_ok(&[
                    "log",
                    "-r",
                    &format!("parents({})", meta_revset()),
                    "--no-graph",
                    "-T",
                    "if(root, \"root\")",
                ])?
                .trim()
                    == "root";
                if !parent_is_root {
                    bail!(
                        "jjq metadata branch ({}) has history but no sequence counter (last_id) — run 'jjq doctor'",
                        JJQ_BOOKMARK
                    );
                }
                true
            }
            Err(e) => return Err(e),
        }
    };

    let temp_dir = TempDir::new()?;
    let workspace_name = format!(
        "jjq-meta-{}-{}",
        std::process::id(),
        WORKSPACE_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    // Arm the guard before `workspace add`: the add is not atomic and can
    // register the workspace, then fail. Drop tolerates a workspace that was
    // never registered.
    let _guard = WorkspaceGuard {
        name: workspace_name.clone(),
    };
    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[&meta_revset()],
    )?;

    if needs_seed {
        fs::write(temp_dir.path().join("last_id"), "0")?;
    }

    let description = mutate(temp_dir.path())?;

    // jj commands run with the workspace as their cwd (never chdir the
    // process): describe snapshots the files written above into @, then the
    // bookmark moves forward to @.
    jj::run_quiet_in(temp_dir.path(), &["describe", "-r", "@", "-m", &description])?;
    if let Err(e) = jj::run_quiet_in(temp_dir.path(), &["bookmark", "set", JJQ_BOOKMARK]) {
        if e.to_string().contains("backwards or sideways") {
            bail!(
                "another writer moved the jjq metadata bookmark ({}) out from under this process — run 'jjq doctor'",
                JJQ_BOOKMARK
            );
        }
        return Err(e);
    }

    Ok(())
}
