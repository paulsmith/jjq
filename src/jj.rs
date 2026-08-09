// ABOUTME: Wrapper module for jj CLI interactions.
// ABOUTME: Provides functions to execute jj commands and parse their output.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// Get a jj config value, returning None if not set.
pub fn config_get(key: &str) -> Result<Option<String>> {
    let output = run_ignore_working_copy(&["config", "get", key])?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

/// Set a jj config value at repo scope.
pub fn config_set_repo(key: &str, value: &str) -> Result<()> {
    run_quiet(&["config", "set", "--repo", key, value])
}

/// Execute a jj command from the process working directory.
pub fn run(args: &[&str]) -> Result<Output> {
    run_from(None, args)
}

/// Execute a jj command without snapshotting or updating the current working
/// copy. Use only for repository/config reads whose arguments are independent
/// of working-copy contents (for example, pinned commit IDs and bookmark
/// inventories). Candidate revsets such as `@` must continue through `run`.
pub fn run_ignore_working_copy(args: &[&str]) -> Result<Output> {
    let mut full = vec!["--ignore-working-copy"];
    full.extend_from_slice(args);
    run(&full)
}

/// Execute a jj command with `dir` as its working directory. jj snapshots
/// the working copy of the workspace containing the command's cwd, so
/// workspace-scoped operations must run through this rather than chdir'ing
/// the process (a process-global chdir breaks concurrent work and strands
/// the process if an error skips the chdir back).
pub fn run_in(dir: &Path, args: &[&str]) -> Result<Output> {
    run_from(Some(dir), args)
}

fn run_from(dir: Option<&Path>, args: &[&str]) -> Result<Output> {
    let mut full_args = vec!["--color=never"];
    full_args.extend_from_slice(args);
    let mut cmd = Command::new("jj");
    cmd.args(&full_args);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd.output().context("failed to execute jj")
}

/// Extract stdout from a jj invocation, failing with stderr context on
/// non-zero exit.
fn expect_success(args: &[&str], output: Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Execute a jj command and return stdout as string, failing on non-zero exit.
pub fn run_ok(args: &[&str]) -> Result<String> {
    expect_success(args, run(args)?)
}

/// `run_ok` for reads which are provably independent of working-copy state.
pub fn run_ok_ignore_working_copy(args: &[&str]) -> Result<String> {
    expect_success(args, run_ignore_working_copy(args)?)
}

/// `run_ok` with `dir` as the command's working directory.
pub fn run_ok_in(dir: &Path, args: &[&str]) -> Result<String> {
    expect_success(args, run_in(dir, args)?)
}

/// Execute a jj command silently, only returning error on failure.
pub fn run_quiet(args: &[&str]) -> Result<()> {
    run_ok(args).map(|_| ())
}

/// `run_quiet` for mutations whose operands are independent of working-copy
/// state and have already been resolved or name exact internal bookmarks.
pub fn run_quiet_ignore_working_copy(args: &[&str]) -> Result<()> {
    run_ok_ignore_working_copy(args).map(|_| ())
}

/// Per-invocation config override disabling jj's short-prefixes index. jj
/// computes that index from revsets.log; a filter there naming a conflicted
/// bookmark as a bare symbol makes every command warn or fail outright. The
/// value is TOML, so the inner double quotes are literal.
pub const NO_SHORT_PREFIXES: &str = r#"revsets.short-prefixes="""#;

/// Execute a jj command with a per-invocation --config override, returning
/// stdout on success. The override never touches the user's repo config, so
/// a crash mid-command cannot destroy their settings.
pub fn run_ok_with_config(config: &str, args: &[&str]) -> Result<String> {
    let mut full = vec!["--config", config];
    full.extend_from_slice(args);
    run_ok(&full)
}

/// `run_ok_with_config` discarding stdout.
pub fn run_quiet_with_config(config: &str, args: &[&str]) -> Result<()> {
    run_ok_with_config(config, args).map(|_| ())
}

/// `run_quiet` with `dir` as the command's working directory.
pub fn run_quiet_in(dir: &Path, args: &[&str]) -> Result<()> {
    run_ok_in(dir, args).map(|_| ())
}

/// Snapshot the working copy of the workspace at `dir` into its @ commit.
/// jj only snapshots the workspace a command runs in; a cheap read-only
/// command inside `dir` forces it, so revset-based operations run from other
/// directories see the workspace's current files.
pub fn snapshot_workspace(dir: &Path) -> Result<()> {
    run_quiet_in(dir, &["file", "list", "-r", "@"])
}

/// Verify we're in a jj repository.
pub fn verify_repo() -> Result<()> {
    let output = run(&["root"])?;
    if !output.status.success() {
        bail!("not in a jj repository");
    }
    Ok(())
}

/// Get the repository root path (cached after first call).
pub fn repo_root() -> Result<PathBuf> {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    if let Some(root) = ROOT.get() {
        return Ok(root.clone());
    }
    let output = run_ok(&["root"])?;
    let root = PathBuf::from(output.trim());
    let _ = ROOT.set(root.clone());
    Ok(root)
}

/// Get the .jj directory shared by all workspaces of this repository
/// (cached after first call).
///
/// In the primary workspace, `<root>/.jj/repo` is a directory and the shared
/// .jj dir is `<root>/.jj` itself. In a secondary workspace, `<root>/.jj/repo`
/// is a file containing the path (possibly relative to `<root>/.jj/`) of the
/// primary workspace's repo directory; the shared .jj dir is that path's
/// parent. State that must be visible across workspaces — lock files and the
/// run log — belongs here, never in a workspace-local .jj directory.
pub fn shared_jj_dir() -> Result<PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Ok(dir.clone());
    }
    let workspace_jj = repo_root()?.join(".jj");
    let repo_path = workspace_jj.join("repo");
    let dir = match fs::metadata(&repo_path) {
        Ok(meta) if meta.is_dir() => workspace_jj,
        Ok(_) => {
            // Known secondary workspace: falling back to the workspace-local
            // .jj here would silently bypass cross-workspace locking, so a
            // pointer that cannot be resolved is a hard error.
            resolve_workspace_pointer(&workspace_jj, &repo_path).map_err(|e| {
                anyhow::anyhow!(
                    "cannot resolve shared repo from workspace pointer {}: {:#}",
                    repo_path.display(),
                    e
                )
            })?
        }
        Err(e) => {
            eprintln!(
                "jjq: warning: cannot resolve shared repo location (cannot stat {}: {}); using workspace-local .jj",
                repo_path.display(),
                e
            );
            workspace_jj
        }
    };
    let _ = DIR.set(dir.clone());
    Ok(dir)
}

/// Resolve the primary workspace's .jj directory from a secondary workspace's
/// repo pointer file, whose contents are the path (possibly relative to the
/// workspace's .jj directory) of the primary repo directory.
fn resolve_workspace_pointer(workspace_jj: &Path, repo_path: &Path) -> Result<PathBuf> {
    let contents = fs::read_to_string(repo_path)?;
    let target = contents.trim();
    if target.is_empty() {
        bail!("pointer file is empty");
    }
    let target = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace_jj.join(target)
    };
    let target = target
        .canonicalize()
        .with_context(|| format!("cannot canonicalize {}", target.display()))?;
    match target.parent() {
        Some(parent) => Ok(parent.to_path_buf()),
        None => bail!("repo directory {} has no parent", target.display()),
    }
}

/// Get the primary workspace root (the directory containing the shared .jj).
pub fn primary_workspace_root() -> Result<PathBuf> {
    let dir = shared_jj_dir()?;
    match dir.parent() {
        Some(parent) => Ok(parent.to_path_buf()),
        None => bail!("shared .jj directory {} has no parent", dir.display()),
    }
}

/// Check if a bookmark exists.
pub fn bookmark_exists(name: &str) -> Result<bool> {
    let output = run_ok_ignore_working_copy(&[
        "bookmark",
        "list",
        "-r",
        &format!("bookmarks(exact:{})", name),
        "-T",
        "name",
    ])?;
    Ok(!output.trim().is_empty())
}

/// Create a bookmark at a revision.
pub fn bookmark_create(name: &str, rev: &str) -> Result<()> {
    run_quiet(&["bookmark", "create", "-r", rev, name])
}

/// Create an internal bookmark at an already resolved target without taking
/// another snapshot of the user's working copy.
pub fn bookmark_create_internal(name: &str, rev: &str) -> Result<()> {
    run_quiet_ignore_working_copy(&["bookmark", "create", "-r", rev, name])
}

/// Delete an internal bookmark. Its target is irrelevant to the mutation.
pub fn bookmark_delete(name: &str) -> Result<()> {
    run_quiet_ignore_working_copy(&["bookmark", "delete", name])
}

/// Rename an internal bookmark without changing its target.
pub fn bookmark_rename(old: &str, new: &str) -> Result<()> {
    run_quiet_ignore_working_copy(&["bookmark", "rename", old, new])
}

/// Check if the jj binary supports --allow-protected on bookmark move.
fn supports_allow_protected() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Ok(output) = run_ignore_working_copy(&["bookmark", "move", "-h"]) else {
            return false;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.contains("allow-protected")
    })
}

/// Move a bookmark from one revision to another (compare-and-swap).
/// Uses --allow-protected if the jj binary supports it.
pub fn bookmark_move(name: &str, from: &str, to: &str) -> Result<()> {
    let mut args = vec!["bookmark", "move"];
    if supports_allow_protected() {
        args.push("--allow-protected");
    }
    args.extend_from_slice(&["--from", from, "--to", to, name]);
    run_quiet_ignore_working_copy(&args)
}

/// List bookmarks matching a glob pattern.
pub fn bookmark_list_glob(pattern: &str) -> Result<Vec<String>> {
    let output = run_ok_ignore_working_copy(&[
        "bookmark",
        "list",
        "-r",
        &format!("bookmarks(glob:\"{}\")", pattern),
        "-T",
        "name ++ \"\\n\"",
    ])?;
    Ok(output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// List all local bookmark names.
pub fn list_bookmarks() -> Result<Vec<String>> {
    let output = run_ok_ignore_working_copy(&["bookmark", "list", "-T", "name ++ \"\\n\""])?;
    Ok(output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Resolve a revset to a single change ID.
pub fn resolve_revset(revset: &str) -> Result<String> {
    let (change_id, _) = require_single_revset(revset, resolve_revset_state(revset, false)?)?;
    Ok(change_id)
}

/// Resolve a revset to both change ID and commit ID.
pub fn resolve_revset_full(revset: &str) -> Result<(String, String)> {
    require_single_revset(revset, resolve_revset_state(revset, false)?)
}

/// Structured result for callers that must distinguish an absent revset from
/// a conflicted one without treating a failed `jj log` process as absence.
#[derive(Debug, PartialEq, Eq)]
pub enum RevsetResolution {
    Absent,
    Single(String, String),
    Multiple,
}

/// Resolve zero, one, or multiple revisions while preserving command errors.
/// `jj log` exits successfully with empty output for a valid revset matching
/// nothing; non-zero status instead means the command itself failed.
pub fn resolve_pinned_revset_full_state(revset: &str) -> Result<RevsetResolution> {
    resolve_revset_state(revset, true)
}

/// Resolve a revset once and classify its complete result. Pinned reads avoid
/// snapshotting the caller's working copy; candidate inputs must remain
/// working-copy-aware.
fn resolve_revset_state(revset: &str, pinned: bool) -> Result<RevsetResolution> {
    let args = [
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        "change_id.short() ++ \" \" ++ commit_id ++ \"\\n\"",
    ];
    let output = if pinned {
        run_ignore_working_copy(&args)?
    } else {
        run(&args)?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().next().unwrap_or("").trim();
        bail!("revset '{}' not found ({})", revset, detail);
    }
    parse_revset_full_output(&String::from_utf8_lossy(&output.stdout))
}

fn require_single_revset(revset: &str, resolution: RevsetResolution) -> Result<(String, String)> {
    match resolution {
        RevsetResolution::Absent => bail!("revset '{}' not found", revset),
        RevsetResolution::Single(change_id, commit_id) => Ok((change_id, commit_id)),
        RevsetResolution::Multiple => bail!("revset '{}' resolves to multiple revisions", revset),
    }
}

fn parse_revset_full_output(output: &str) -> Result<RevsetResolution> {
    let mut revisions = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let (change_id, commit_id) = line
            .trim()
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("unexpected output format from jj log"))?;
        if change_id.is_empty() || commit_id.is_empty() || commit_id.contains(' ') {
            bail!("unexpected output format from jj log");
        }
        revisions.push((change_id.to_string(), commit_id.to_string()));
    }
    Ok(match revisions.len() {
        0 => RevsetResolution::Absent,
        1 => {
            let (change_id, commit_id) = revisions.pop().unwrap();
            RevsetResolution::Single(change_id, commit_id)
        }
        _ => RevsetResolution::Multiple,
    })
}

/// Get the commit ID for a revision.
pub fn get_commit_id(revset: &str) -> Result<String> {
    run_ok(&["log", "-r", revset, "--no-graph", "-T", "commit_id"]).map(|s| s.trim().to_string())
}

/// Get the full description of a revision.
pub fn get_description(revset: &str) -> Result<String> {
    run_ok(&["log", "-r", revset, "--no-graph", "-T", "description"])
}

/// Check if a revision has conflicts.
pub fn has_conflicts(revset: &str) -> Result<bool> {
    let output = run_ok(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        "if(conflict, \"yes\")",
    ])?;
    Ok(!output.trim().is_empty())
}

/// List conflicting file paths in a revision.
pub fn conflict_paths(revset: &str) -> Result<Vec<String>> {
    let output = run_ok(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        r#"conflicted_files.map(|e| e.path() ++ "\n").join("")"#,
    ])?;
    Ok(output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Check if two revisions have the same tree (no diff between them).
pub fn trees_match(from: &str, to: &str) -> Result<bool> {
    let output = run_ok(&["diff", "--from", from, "--to", to, "--summary"])?;
    Ok(output.trim().is_empty())
}

/// Move the invoking workspace's working copy to a new empty commit on top
/// of the current working-copy commit (plain `jj new`).
pub fn new_working_copy() -> Result<()> {
    run_quiet(&["new"])
}

/// Names of workspaces whose working-copy commit is in `revset`.
pub fn workspaces_at(revset: &str) -> Result<Vec<String>> {
    let output = run_ok(&[
        "log",
        "-r",
        &format!("({}) & working_copies()", revset),
        "--no-graph",
        "-T",
        "working_copies ++ \"\\n\"",
    ])?;
    Ok(output
        .split_whitespace()
        .map(|s| s.trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Create a new revision with given parent(s).
pub fn new_rev(parents: &[&str]) -> Result<String> {
    let mut args = vec!["new", "--no-edit"];
    for p in parents {
        args.push("-r");
        args.push(p);
    }
    let output = run(&args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj new failed: {}", stderr.trim());
    }
    // jj outputs status messages to stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Parse change_id from output like "Created new commit xopxuxzw ..."
    for line in stderr.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // "Created new commit <change_id> ..."
        if parts.len() >= 4 && parts[0] == "Created" && parts[1] == "new" && parts[2] == "commit" {
            return Ok(parts[3].to_string());
        }
    }
    bail!("failed to parse change ID from jj new output: {}", stderr)
}

/// Describe a revision.
pub fn describe(rev: &str, message: &str) -> Result<()> {
    run_quiet(&["desc", "-r", rev, "-m", message])
}

/// Abandon a revision.
pub fn abandon(rev: &str) -> Result<()> {
    run_quiet(&["abandon", rev])
}

/// Duplicate the commit range destination..revset onto the destination,
/// returning all new change IDs (the last one is the tip/candidate).
/// This handles commit chains: if revset has ancestors between it and
/// destination, those intermediate commits are also duplicated.
/// Parses stderr for: "Duplicated <hash> as <new_change_id> <new_hash> ..."
pub fn duplicate_onto(revset: &str, destination: &str) -> Result<Vec<String>> {
    let range = format!("{}..{}", destination, revset);
    let output = run(&["duplicate", &range, "--onto", destination])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj duplicate failed: {}", stderr.trim());
    }
    // jj duplicate outputs to stderr; one line per duplicated commit.
    // Collect all duplicated change IDs (last one is the tip).
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut change_ids = Vec::new();
    for line in stderr.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // "Duplicated <old_hash> as <new_change_id> <new_hash> ..."
        if parts.len() >= 4 && parts[0] == "Duplicated" && parts[2] == "as" {
            change_ids.push(parts[3].to_string());
        }
    }
    if change_ids.is_empty() {
        bail!(
            "failed to parse change ID from jj duplicate output: {}",
            stderr.trim()
        );
    }
    Ok(change_ids)
}

/// Edit a revision (set it as the working copy of the workspace at `dir`).
pub fn edit(dir: &Path, rev: &str) -> Result<()> {
    run_quiet_in(dir, &["edit", rev])
}

/// Rebase a revision and its ancestors (up to destination) onto the destination.
/// Uses `jj rebase -b` which rebases the "branch" — all revisions in the
/// range (destination..source) plus their descendants.
pub fn rebase_branch_onto(source: &str, destination: &str) -> Result<()> {
    run_quiet(&["rebase", "-b", source, "-d", destination])
}

/// Show file contents from an exact, already-pinned commit without snapshotting
/// the caller's working copy.
pub fn file_show_pinned(path: &str, commit_id: &str) -> Result<String> {
    run_ok_ignore_working_copy(&["file", "show", path, "-r", commit_id])
}

const FILE_SHOW_FRAME: &str = "\0JJQ-FILE\0";
const FILE_SHOW_TEMPLATE: &str = r#""\0JJQ-FILE\0" ++ path ++ "\0""#;

/// Show every file selected by `fileset` in one jj invocation.
///
/// `jj file show` renders its template immediately before each file's bytes,
/// so a NUL-delimited path header gives us a narrow, machine-readable stream.
/// jjq's metadata values are text (and JSON escapes embedded NULs); reject a
/// raw NUL in contents rather than ambiguously treating it as framing.
pub fn file_show_many_pinned(fileset: &str, commit_id: &str) -> Result<Vec<(String, String)>> {
    let args = [
        "file",
        "show",
        "-r",
        commit_id,
        "-T",
        FILE_SHOW_TEMPLATE,
        fileset,
    ];
    let output = run_ignore_working_copy(&args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    let stdout =
        String::from_utf8(output.stdout).context("jj file show returned non-UTF-8 data")?;
    parse_file_show_many(&stdout)
}

fn parse_file_show_many(output: &str) -> Result<Vec<(String, String)>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }

    let mut rest = output;
    let mut files = Vec::new();
    let mut seen_paths = HashSet::new();
    while !rest.is_empty() {
        rest = rest.strip_prefix(FILE_SHOW_FRAME).ok_or_else(|| {
            anyhow::anyhow!("invalid bulk file-show framing: missing file header")
        })?;
        let path_end = rest.find('\0').ok_or_else(|| {
            anyhow::anyhow!("invalid bulk file-show framing: unterminated file path")
        })?;
        let path = &rest[..path_end];
        if path.is_empty() {
            bail!("invalid bulk file-show framing: empty file path");
        }
        rest = &rest[path_end + 1..];

        let content_end = rest.find(FILE_SHOW_FRAME).unwrap_or(rest.len());
        let contents = &rest[..content_end];
        if contents.contains('\0') {
            bail!(
                "invalid bulk file-show framing: NUL in contents for {}",
                path
            );
        }
        if !seen_paths.insert(path.to_string()) {
            bail!("invalid bulk file-show framing: duplicate path {}", path);
        }
        files.push((path.to_string(), contents.to_string()));
        rest = &rest[content_end..];
    }
    Ok(files)
}

/// Create a workspace.
pub fn workspace_add(path: &str, name: &str, parents: &[&str]) -> Result<()> {
    let mut args = vec!["workspace", "add"];
    for p in parents {
        args.push("-r");
        args.push(p);
    }
    args.push("--name");
    args.push(name);
    args.push(path);
    run_quiet(&args)
}

/// Create and edit a fresh working-copy commit with its description already
/// set, allowing the eventual publishing command to snapshot file changes
/// without a separate `jj describe` invocation.
pub fn workspace_new_described(dir: &Path, parents: &[&str], description: &str) -> Result<()> {
    let mut args = vec!["new"];
    for parent in parents {
        args.push("-r");
        args.push(parent);
    }
    args.push("-m");
    args.push(description);
    run_quiet_in(dir, &args)
}

/// Whether a workspace with exactly `name` is registered in the repository.
pub fn workspace_exists(name: &str) -> Result<bool> {
    Ok(workspace_list()?.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|listed| listed.trim_end_matches(':') == name)
    }))
}

/// Refresh workspace records whose operation is older than the repository's
/// current operation.
pub fn workspace_update_stale() -> Result<()> {
    run_quiet(&["workspace", "update-stale"])
}

/// Forget a workspace, updating stale state first if needed.
pub fn workspace_forget(name: &str) -> Result<()> {
    let output = run(&["workspace", "forget", name])?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("stale") {
        let _ = run(&["workspace", "update-stale"]);
        return run_quiet(&["workspace", "forget", name]);
    }
    bail!("jj workspace forget {} failed: {}", name, stderr.trim())
}

/// List all workspaces (raw output from jj workspace list).
pub fn workspace_list() -> Result<String> {
    run_ok_ignore_working_copy(&["workspace", "list"])
}

#[cfg(test)]
mod tests {
    use super::{parse_file_show_many, parse_revset_full_output, RevsetResolution};

    #[test]
    fn parses_full_revset_resolution_states() {
        assert_eq!(
            parse_revset_full_output("").unwrap(),
            RevsetResolution::Absent
        );
        assert_eq!(
            parse_revset_full_output("change commit\n").unwrap(),
            RevsetResolution::Single("change".to_string(), "commit".to_string())
        );
        assert_eq!(
            parse_revset_full_output("one commit1\ntwo commit2\n").unwrap(),
            RevsetResolution::Multiple
        );
        assert!(parse_revset_full_output("malformed\n").is_err());
        assert!(parse_revset_full_output("change commit extra\n").is_err());
    }

    #[test]
    fn parses_bulk_file_show_framing() {
        let output =
            "\0JJQ-FILE\0config/check_command\0cargo test\n\0JJQ-FILE\0config/strategy\0rebase";
        assert_eq!(
            parse_file_show_many(output).unwrap(),
            vec![
                (
                    "config/check_command".to_string(),
                    "cargo test\n".to_string()
                ),
                ("config/strategy".to_string(), "rebase".to_string()),
            ]
        );
        assert!(parse_file_show_many("").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_bulk_file_show_framing() {
        assert!(parse_file_show_many("config/key\0value").is_err());
        assert!(parse_file_show_many("\0JJQ-FILE\0\0value").is_err());
        assert!(parse_file_show_many("\0JJQ-FILE\0config/key").is_err());
        assert!(parse_file_show_many("\0JJQ-FILE\0config/key\0bad\0value").is_err());
        assert!(
            parse_file_show_many("\0JJQ-FILE\0config/key\0one\0JJQ-FILE\0config/key\0two").is_err()
        );
    }
}
