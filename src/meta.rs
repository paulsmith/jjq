// ABOUTME: Single-writer discipline for the jjq metadata branch (jjq/_/_).
// ABOUTME: All metadata writes flow through described transactions under one lock.

// Lock-order invariant: a process may acquire the `meta` lock while holding
// the `run` lock (run_one records workspace metadata mid-run), but no code
// path may acquire the `run` lock while holding `meta`. Keeping the order
// run -> meta acyclic makes deadlock impossible.

use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::lock::Lock;

/// The jjq metadata bookmark name.
pub const JJQ_BOOKMARK: &str = "jjq/_/_";

/// Reserved jj workspace used for all metadata commits. It lives under the
/// shared repository's `.jj/jjq/` directory and is recreated if its durable
/// registration or working-copy directory disappears.
pub const META_WORKSPACE_NAME: &str = "jjq-meta";

/// Fixed working-copy path for the persistent metadata workspace.
pub fn metadata_workspace_path() -> Result<PathBuf> {
    Ok(jj::shared_jj_dir()?.join("jjq").join("meta-workspace"))
}

/// Verify that the reserved workspace registration, when present, points at
/// jjq's fixed internal cache path. `jjq-meta` was once a valid user-chosen
/// workspace name, so ownership cannot be inferred from the name alone.
///
/// Returns whether the owned registration exists. A colliding user workspace
/// is preserved and reported rather than forgotten or cleaned.
pub fn metadata_workspace_registration_owned() -> Result<bool> {
    if !jj::workspace_exists(META_WORKSPACE_NAME)? {
        return Ok(false);
    }

    let registered =
        jj::run_ok_ignore_working_copy(&["workspace", "root", "--name", META_WORKSPACE_NAME])?;
    let registered = PathBuf::from(registered.trim());
    let expected = metadata_workspace_path()?;
    let same_path = registered == expected
        || match (fs::canonicalize(&registered), fs::canonicalize(&expected)) {
            (Ok(registered), Ok(expected)) => registered == expected,
            _ => false,
        };
    if !same_path {
        bail!(
            "workspace name '{}' is already used by a user workspace at {} — rename that workspace before retrying jjq",
            META_WORKSPACE_NAME,
            registered.display()
        );
    }

    Ok(true)
}

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
/// briefly beats failing a push that raced a config write. Exposed so
/// `doctor --fix` can hold the same lock while repairing the metadata
/// bookmark in place; everything else writes via a described metadata transaction.
pub fn acquire() -> Result<Lock> {
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

/// Run `mutate` in the persistent metadata workspace and commit
/// the result, serialized under the repo-scoped `meta` lock. This is the only
/// way jjq writes to the metadata branch: one lock, one writer, one bookmark
/// move at a time.
///
/// Commit a metadata mutation whose operation-log description is known before
/// its files are written. `mutate` receives the workspace root, writes files
/// via absolute paths, and returns only success or failure. The fresh
/// working-copy child is created with the description, and `bookmark set`
/// snapshots and publishes the file changes in one jj command. The branch —
/// with its `last_id` seed — is created under the lock on first use. Each
/// mutation starts from a fresh child of the pinned metadata commit, so a
/// crash leaves only unbookmarked work; the next writer starts from the
/// authoritative bookmark again. The process working directory is never
/// changed.
pub fn with_described_meta_workspace(
    description: &str,
    mutate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    with_meta_workspace_mode(false, description, mutate)
}

/// Initial metadata transaction with an operation-log description known
/// before mutation. Completed initialization is still rejected under the
/// metadata lock, while an interrupted bare root remains recoverable.
pub fn with_initial_described_meta_workspace(
    description: &str,
    mutate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    with_meta_workspace_mode(true, description, mutate)
}

fn with_meta_workspace_mode(
    require_new_branch: bool,
    description: &str,
    mutate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let _lock = acquire()?;

    // Resolve existence and the exact parent with one `jj log` call. Empty
    // output means first use; command errors are preserved as errors.
    let (created, meta_parent) = match jj::resolve_pinned_revset_full_state(&meta_revset())? {
        jj::RevsetResolution::Absent => {
            let change_id = jj::new_rev(&["root()"])?;
            jj::describe(&change_id, "jjq metadata root")?;
            jj::run_quiet(&["bookmark", "create", "-r", &change_id, JJQ_BOOKMARK])?;
            (true, change_id)
        }
        jj::RevsetResolution::Single(_, commit_id) => (false, commit_id),
        jj::RevsetResolution::Multiple => {
            bail!(
                "jjq metadata bookmark ({}) is conflicted — run 'jjq doctor'",
                JJQ_BOOKMARK
            );
        }
    };

    let workspace = prepare_metadata_workspace(&meta_parent, description)?;

    // The fresh child's tree is exactly meta_parent, so checking its local
    // last_id avoids a separate `jj file show` on every healthy transaction.
    // Missing last_id is recoverable only for the just-created/bare metadata
    // root; a branch with real history is corrupt and must not be reseeded.
    let needs_seed = if created {
        true
    } else {
        match fs::read(workspace.join("last_id")) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent_is_root = jj::run_ok_ignore_working_copy(&[
                    "log",
                    "-r",
                    &format!("parents({})", meta_parent),
                    "--no-graph",
                    "-T",
                    "if(root, \"root\")",
                ])?
                .trim()
                    == "root";
                if !parent_is_root {
                    return abort_prepared_transaction(
                        &workspace,
                        &meta_parent,
                        anyhow::anyhow!(
                            "jjq metadata branch ({}) has history but no sequence counter (last_id) — run 'jjq doctor'",
                            JJQ_BOOKMARK
                        ),
                    );
                }
                true
            }
            Err(error) => {
                return abort_prepared_transaction(&workspace, &meta_parent, error.into());
            }
        }
    };
    if require_new_branch && !created && !needs_seed {
        return abort_prepared_transaction(
            &workspace,
            &meta_parent,
            ExitError::new(
                exit_codes::USAGE,
                "jjq is already initialized. Use 'jjq config' to change settings.",
            )
            .into(),
        );
    }

    if needs_seed {
        fs::write(workspace.join("last_id"), "0")?;
    }

    mutate(&workspace)?;

    // jj commands run with the workspace as their cwd (never chdir the
    // process). The child already carries its description, so bookmark set
    // snapshots and publishes the files in one command.
    if let Err(e) = jj::run_quiet_in(&workspace, &["bookmark", "set", JJQ_BOOKMARK]) {
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

/// Abort before the caller's mutation ran, discarding the clean unpublished
/// child when possible. If cleanup itself fails, the next transaction's
/// unconditional recovery pass retries it.
fn abort_prepared_transaction(
    workspace: &Path,
    meta_parent: &str,
    error: anyhow::Error,
) -> Result<()> {
    let _ = abandon_interrupted_metadata_child(workspace, meta_parent);
    Err(error)
}

/// Return a clean child of `meta_parent` in the persistent metadata workspace.
/// A prior crash may have left dirty or described work at `@`; `jj new -r`
/// snapshots that unbookmarked work, then switches to a fresh child of the
/// authoritative pinned parent without publishing the abandoned mutation.
fn prepare_metadata_workspace(meta_parent: &str, description: &str) -> Result<PathBuf> {
    let path = metadata_workspace_path()?;
    let pointer = path.join(".jj");

    if pointer.is_dir() {
        match new_metadata_child(&path, meta_parent, description) {
            Ok(()) => return Ok(path),
            Err(_) => {
                // A workspace can become stale after a crash or an operation
                // merged from another process. Refresh registered state once
                // before recreating this reserved, disposable workspace.
                if jj::workspace_exists(META_WORKSPACE_NAME)? {
                    let _ = jj::workspace_update_stale();
                    if new_metadata_child(&path, meta_parent, description).is_ok() {
                        return Ok(path);
                    }
                }
            }
        }
    }

    recreate_metadata_workspace(&path, meta_parent, description)?;
    Ok(path)
}

fn new_metadata_child(path: &Path, meta_parent: &str, description: &str) -> Result<()> {
    abandon_interrupted_metadata_child(path, meta_parent)?;
    jj::workspace_new_described(path, &[meta_parent], description)
}

/// Discard only the persistent workspace child left by an interrupted
/// metadata transaction. Running `jj log` in this workspace first snapshots
/// any dirty files from the crashed writer. The revset deliberately excludes
/// bookmarked commits and requires descent from the pinned authoritative
/// parent, so an older metadata commit in a stale workspace and the current
/// bookmarked parent are never abandoned.
fn abandon_interrupted_metadata_child(path: &Path, meta_parent: &str) -> Result<()> {
    let revset = format!("@ & descendants({}) & ~bookmarks()", meta_parent);
    let output = jj::run_ok_in(
        path,
        &[
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )?;
    let mut commits = output.lines().filter(|line| !line.is_empty());
    let Some(commit) = commits.next() else {
        return Ok(());
    };
    if commits.next().is_some() {
        bail!("persistent metadata workspace resolves to multiple commits");
    }
    jj::run_quiet_in(path, &["abandon", commit])
}

fn recreate_metadata_workspace(path: &Path, meta_parent: &str, description: &str) -> Result<()> {
    if metadata_workspace_registration_owned()? {
        jj::workspace_forget(META_WORKSPACE_NAME)?;
    }
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("metadata workspace path has no parent"))?;
    fs::create_dir_all(parent)?;
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("metadata workspace path is not valid UTF-8"))?;
    jj::run_quiet(&[
        "workspace",
        "add",
        "-r",
        meta_parent,
        "--name",
        META_WORKSPACE_NAME,
        "-m",
        description,
        path,
    ])
}
