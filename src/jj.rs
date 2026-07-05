// ABOUTME: Wrapper module for jj CLI interactions.
// ABOUTME: Provides functions to execute jj commands and parse their output.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// Get a jj config value, returning None if not set.
pub fn config_get(key: &str) -> Result<Option<String>> {
    let output = run(&["config", "get", key])?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    } else {
        Ok(None)
    }
}

/// Set a jj config value at repo scope.
pub fn config_set_repo(key: &str, value: &str) -> Result<()> {
    run_quiet(&["config", "set", "--repo", key, value])
}

/// Execute a jj command and return the output.
pub fn run(args: &[&str]) -> Result<Output> {
    let mut full_args = vec!["--color=never"];
    full_args.extend_from_slice(args);
    let output = Command::new("jj")
        .args(&full_args)
        .output()
        .context("failed to execute jj")?;
    Ok(output)
}

/// Execute a jj command and return stdout as string, failing on non-zero exit.
pub fn run_ok(args: &[&str]) -> Result<String> {
    let output = run(args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Execute a jj command silently, only returning error on failure.
pub fn run_quiet(args: &[&str]) -> Result<()> {
    let output = run(args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
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
    let output = run_ok(&[
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

/// Delete a bookmark.
pub fn bookmark_delete(name: &str) -> Result<()> {
    run_quiet(&["bookmark", "delete", name])
}

/// Check if the jj binary supports --allow-protected on bookmark move.
fn supports_allow_protected() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Ok(output) = run(&["bookmark", "move", "-h"]) else {
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
    run_quiet(&args)
}

/// List bookmarks matching a glob pattern.
pub fn bookmark_list_glob(pattern: &str) -> Result<Vec<String>> {
    let output = run_ok(&[
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
    let output = run_ok(&["bookmark", "list", "-T", "name ++ \"\\n\""])?;
    Ok(output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Resolve a revset to a single change ID.
pub fn resolve_revset(revset: &str) -> Result<String> {
    let output = run(&["log", "-r", revset, "--no-graph", "-T", "change_id.short()"])?;
    if !output.status.success() {
        bail!("revset '{}' not found", revset);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let change_id = stdout.trim();
    if change_id.is_empty() {
        bail!("revset '{}' not found", revset);
    }
    // Check for multiple matches (output would have multiple lines)
    if change_id.contains('\n') {
        bail!("revset '{}' resolves to multiple revisions", revset);
    }
    Ok(change_id.to_string())
}

/// Resolve a revset to both change ID and commit ID.
pub fn resolve_revset_full(revset: &str) -> Result<(String, String)> {
    let output = run(&[
        "log", "-r", revset, "--no-graph", "-T",
        "change_id.short() ++ \" \" ++ commit_id",
    ])?;
    if !output.status.success() {
        bail!("revset '{}' not found", revset);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    if line.is_empty() {
        bail!("revset '{}' not found", revset);
    }
    if line.contains('\n') {
        bail!("revset '{}' resolves to multiple revisions", revset);
    }
    let (change_id, commit_id) = line.split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("unexpected output format from jj log"))?;
    Ok((change_id.to_string(), commit_id.to_string()))
}

/// Get the commit ID for a revision.
pub fn get_commit_id(revset: &str) -> Result<String> {
    run_ok(&["log", "-r", revset, "--no-graph", "-T", "commit_id"])
        .map(|s| s.trim().to_string())
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

/// Edit a revision (set it as working copy in current workspace).
pub fn edit(rev: &str) -> Result<()> {
    run_quiet(&["edit", rev])
}

/// Rebase a revision and its ancestors (up to destination) onto the destination.
/// Uses `jj rebase -b` which rebases the "branch" — all revisions in the
/// range (destination..source) plus their descendants.
pub fn rebase_branch_onto(source: &str, destination: &str) -> Result<()> {
    run_quiet(&["rebase", "-b", source, "-d", destination])
}

/// Show file contents from a revision.
pub fn file_show(path: &str, rev: &str) -> Result<String> {
    run_ok(&["file", "show", path, "-r", rev])
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
    run_ok(&["workspace", "list"])
}

