// ABOUTME: Wrapper module for jj CLI interactions.
// ABOUTME: Provides functions to execute jj commands and parse their output.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command, Output};

/// Get a jj config value, returning None if not set.
pub fn config_get(key: &str) -> Result<Option<String>> {
    let output = run(&["config", "get", key])?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    } else {
        Ok(None)
    }
}

/// Execute a jj command and return the output.
pub fn run(args: &[&str]) -> Result<Output> {
    let output = Command::new("jj")
        .args(args)
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

/// Get the repository root path.
pub fn repo_root() -> Result<PathBuf> {
    let output = run_ok(&["root"])?;
    Ok(PathBuf::from(output.trim()))
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

/// Move a bookmark to current working copy.
pub fn bookmark_move(name: &str) -> Result<()> {
    run_quiet(&["bookmark", "move", name])
}

/// Set a bookmark at a revision (like move but for specific rev).
#[allow(dead_code)]
pub fn bookmark_set(name: &str, rev: &str) -> Result<()> {
    run_quiet(&["bookmark", "set", "-r", rev, name])
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

/// Forget a workspace.
pub fn workspace_forget(name: &str) -> Result<()> {
    run_quiet(&["workspace", "forget", name])
}

/// List all workspaces (raw output from jj workspace list).
pub fn workspace_list() -> Result<String> {
    run_ok(&["workspace", "list"])
}

