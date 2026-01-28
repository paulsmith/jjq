// ABOUTME: End-to-end tests for jjq using insta snapshot testing.
// ABOUTME: Tests the full merge queue workflow including conflict handling.

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use tempfile::TempDir;

/// Test fixture for a jj repository with jjq.
struct TestRepo {
    /// Kept to ensure temp directory lives as long as TestRepo.
    #[allow(dead_code)]
    dir: TempDir,
    path: PathBuf,
}

impl TestRepo {
    /// Create a new empty jj repository.
    fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().to_path_buf();

        // Initialize jj repo
        run_jj(&path, &["git", "init", "."]);

        TestRepo { dir, path }
    }

    /// Create a test repository with main branch and optional PRs.
    fn with_go_project() -> Self {
        let repo = Self::new();

        // Create go.mod
        fs::write(
            repo.path.join("go.mod"),
            "module example/jjdemo\n\ngo 1.24\n",
        )
        .unwrap();

        // Create main.go
        fs::write(
            repo.path.join("main.go"),
            r#"package main

import "fmt"

func main() {
	fmt.Println("Hello, world!")
}
"#,
        )
        .unwrap();

        // Create main_test.go
        fs::write(
            repo.path.join("main_test.go"),
            r#"package main_test

import (
	"os/exec"
	"testing"
)

func TestMain(t *testing.T) {
	cmd := exec.Command("go", "run", ".")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatal(err)
	}
	want := "Hello, world!\n"
	if string(out) != want {
		t.Errorf("want %q, got %q", want, string(out))
	}
}
"#,
        )
        .unwrap();

        // Create Makefile
        fs::write(
            repo.path.join("Makefile"),
            "all: test\ntest:\n\tgo test -v ./...\n",
        )
        .unwrap();

        // Format and commit
        run_in_dir(&repo.path, "go", &["fmt", "./..."]);
        run_jj(&repo.path, &["desc", "-m", "initial"]);
        run_jj(&repo.path, &["bookmark", "create", "main"]);

        repo
    }

    /// Create a full test repo with all 4 PRs.
    fn with_prs() -> Self {
        let repo = Self::with_go_project();

        // PR1: Add greeting package
        run_jj(&repo.path, &["new", "-m", "add greeting pkg", "main"]);
        fs::create_dir_all(repo.path.join("say")).unwrap();
        fs::write(
            repo.path.join("say/greet.go"),
            r#"package say

func Greet(name string) string {
	return "Hello, " + name + "!"
}
"#,
        )
        .unwrap();
        fs::write(
            repo.path.join("main.go"),
            r#"package main

import (
	"fmt"

	"example/jjdemo/say"
)

func main() {
	fmt.Println(say.Greet("world"))
}
"#,
        )
        .unwrap();
        run_in_dir(&repo.path, "go", &["fmt", "./..."]);
        run_jj(&repo.path, &["bookmark", "create", "pr1"]);

        // PR2: Add goodbye
        run_jj(&repo.path, &["new", "-m", "add goodbye", "main"]);
        fs::create_dir_all(repo.path.join("say")).unwrap();
        fs::write(
            repo.path.join("say/bye.go"),
            r#"package say

func Bye() string {
	return "Goodbye."
}
"#,
        )
        .unwrap();
        fs::write(
            repo.path.join("main.go"),
            r#"package main

import (
	"fmt"

	"example/jjdemo/say"
)

func main() {
	fmt.Println("Hello, world!")
	fmt.Println(say.Bye())
}
"#,
        )
        .unwrap();
        fs::write(
            repo.path.join("main_test.go"),
            r#"package main_test

import (
	"os/exec"
	"testing"
)

func TestMain(t *testing.T) {
	cmd := exec.Command("go", "run", ".")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatal(err)
	}
	want := "Hello, world!\nGoodbye.\n"
	if string(out) != want {
		t.Errorf("want %q, got %q", want, string(out))
	}
}
"#,
        )
        .unwrap();
        run_in_dir(&repo.path, "go", &["fmt", "./..."]);
        run_jj(&repo.path, &["bookmark", "create", "pr2"]);

        // PR3: Add comment
        run_jj(&repo.path, &["new", "-m", "add comment", "main"]);
        fs::write(
            repo.path.join("main.go"),
            r#"package main

import "fmt"

func main() {
	// say hi
	fmt.Println("Hello, world!")
}
"#,
        )
        .unwrap();
        run_in_dir(&repo.path, "go", &["fmt", "./..."]);
        run_jj(&repo.path, &["bookmark", "create", "pr3"]);

        // PR4: Add readme
        run_jj(&repo.path, &["new", "-m", "add readme", "main"]);
        fs::write(repo.path.join("README.md"), "# jjq demo\n").unwrap();
        run_jj(&repo.path, &["bookmark", "create", "pr4"]);

        // Return to main
        run_jj(&repo.path, &["new", "main"]);

        repo
    }

    /// Get a jjq Command configured for this repo.
    fn jjq(&self) -> Command {
        #[allow(deprecated)]
        let mut cmd = Command::cargo_bin("jjq").unwrap();
        cmd.current_dir(&self.path);
        cmd.env("NON_INTERACTIVE", "1");
        cmd
    }

    /// Run jjq and return normalized output for snapshots.
    fn jjq_output(&self, args: &[&str]) -> String {
        let output = self
            .jjq()
            .args(args)
            .output()
            .expect("failed to run jjq");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let combined = format!("{}{}", stdout, stderr);
        normalize_output(&combined, &self.path)
    }

    /// Run jjq expecting success.
    fn jjq_success(&self, args: &[&str]) -> String {
        let output = self
            .jjq()
            .args(args)
            .assert()
            .success()
            .get_output()
            .clone();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let combined = format!("{}{}", stdout, stderr);
        normalize_output(&combined, &self.path)
    }

    /// Run jjq expecting failure.
    fn jjq_failure(&self, args: &[&str]) -> String {
        let output = self
            .jjq()
            .args(args)
            .assert()
            .failure()
            .get_output()
            .clone();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let combined = format!("{}{}", stdout, stderr);
        normalize_output(&combined, &self.path)
    }

    /// Get the path.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Check if a file exists on a revision.
    fn jj_file_exists(&self, path: &str, rev: &str) -> bool {
        let output = process::Command::new("jj")
            .current_dir(&self.path)
            .args(["file", "show", path, "-r", rev])
            .output()
            .expect("failed to run jj");
        output.status.success()
    }

    /// Run jjq with additional environment variables.
    fn jjq_with_env(&self, args: &[&str], env_vars: &[(&str, &str)]) -> String {
        #[allow(deprecated)]
        let mut cmd = Command::cargo_bin("jjq").unwrap();
        cmd.current_dir(&self.path);
        cmd.env("NON_INTERACTIVE", "1");
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        let output = cmd.args(args).output().expect("failed to run jjq");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let combined = format!("{}{}", stdout, stderr);
        normalize_output(&combined, &self.path)
    }
}

/// Run a jj command in the given directory.
fn run_jj(dir: &Path, args: &[&str]) -> String {
    let output = process::Command::new("jj")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run jj");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("jj {:?} failed: {}", args, stderr);
    }

    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Run a command in the given directory.
fn run_in_dir(dir: &Path, cmd: &str, args: &[&str]) -> String {
    let output = process::Command::new(cmd)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("failed to run {}", cmd));

    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Normalize output for snapshot comparison.
/// Replaces change IDs, commit IDs, paths, and other variable content.
fn normalize_output(output: &str, repo_path: &Path) -> String {
    let mut result = output.to_string();

    // Replace the repo path with a placeholder
    let repo_str = repo_path.to_string_lossy();
    result = result.replace(&*repo_str, "<REPO>");

    // Replace temp directory paths (various formats)
    let re_temp = regex::Regex::new(r"/var/folders/[^\s]+|/tmp/[^\s]+|/private/var/folders/[^\s]+")
        .unwrap();
    result = re_temp.replace_all(&result, "<TEMP_PATH>").to_string();

    // Replace change IDs (12 lowercase letters)
    let re_change_id = regex::Regex::new(r"\b[a-z]{12}\b").unwrap();
    result = re_change_id
        .replace_all(&result, "<CHANGE_ID>")
        .to_string();

    // Replace short change IDs in "now at <id>" pattern
    let re_now_at = regex::Regex::new(r"\(now at [a-z]+\)").unwrap();
    result = re_now_at
        .replace_all(&result, "(now at <CHANGE_ID>)")
        .to_string();

    // Trim trailing whitespace from lines
    result = result
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    // Ensure single trailing newline
    if !result.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }

    result
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn test_status_uninitialized() {
    let repo = TestRepo::new();
    let output = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(output, @"jjq: jjq not initialized (run 'jjq push <revset>' to start)");
}

#[test]
fn test_push_no_trunk() {
    let repo = TestRepo::new();
    // Create a commit but no main bookmark
    fs::write(repo.path().join("file.txt"), "content").unwrap();
    run_jj(repo.path(), &["desc", "-m", "test commit"]);

    let output = repo.jjq_failure(&["push", "@"]);
    insta::assert_snapshot!(output, @"jjq: trunk bookmark 'main' not found");
}

#[test]
fn test_config_show_all() {
    let repo = TestRepo::with_go_project();
    // Push something to initialize jjq
    repo.jjq_success(&["push", "main"]);
    let output = repo.jjq_success(&["config"]);
    insta::assert_snapshot!(output, @r"
    trunk_bookmark = main
    check_command = (not set)
    max_failures = 3
    ");
}

#[test]
fn test_config_set_and_get() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["push", "main"]);

    let set_output = repo.jjq_success(&["config", "check_command", "make test"]);
    insta::assert_snapshot!(set_output, @"jjq: check_command = make test");

    let get_output = repo.jjq_success(&["config", "check_command"]);
    insta::assert_snapshot!(get_output, @"make test");
}

#[test]
fn test_config_invalid_key() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["push", "main"]);

    let output = repo.jjq_failure(&["config", "invalid_key"]);
    insta::assert_snapshot!(output, @r"
    jjq: unknown config key: invalid_key
    valid keys: trunk_bookmark, check_command, max_failures
    ");
}

#[test]
fn test_push_and_status() {
    let repo = TestRepo::with_go_project();

    // Create a branch to push
    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);

    let push_output = repo.jjq_success(&["push", "feature"]);
    insta::assert_snapshot!(push_output, @"jjq: revision 'feature' queued at 1");

    let status_output = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_output, @r"
    jjq: Queued:
      1: <CHANGE_ID> test feature
    ");
}

#[test]
fn test_push_conflict_detection() {
    let repo = TestRepo::with_go_project();

    // Create a branch that conflicts with main
    run_jj(repo.path(), &["new", "-m", "conflicting change", "main"]);
    // Modify main.go in a way that will conflict
    fs::write(
        repo.path().join("main.go"),
        r#"package main

import "fmt"

func main() {
	fmt.Println("Different content!")
}
"#,
    )
    .unwrap();
    run_jj(repo.path(), &["bookmark", "create", "conflict-branch"]);

    // First push succeeds
    repo.jjq_success(&["push", "conflict-branch"]);

    // Modify main directly to create conflict scenario
    run_jj(repo.path(), &["edit", "main"]);
    fs::write(
        repo.path().join("main.go"),
        r#"package main

import "fmt"

func main() {
	fmt.Println("Modified main!")
}
"#,
    )
    .unwrap();
    run_jj(repo.path(), &["desc", "-m", "modify main"]);

    // Create another branch from the modified main
    run_jj(repo.path(), &["new", "-m", "another change", "@"]);
    fs::write(
        repo.path().join("main.go"),
        r#"package main

import "fmt"

func main() {
	fmt.Println("Yet another change!")
}
"#,
    )
    .unwrap();
    run_jj(repo.path(), &["bookmark", "create", "another-branch"]);
    run_jj(repo.path(), &["bookmark", "move", "main", "--to", "@-"]);

    // This should fail with conflict detection
    let output = repo.jjq_failure(&["push", "conflict-branch"]);
    insta::assert_snapshot!(output, @r"
    jjq: revision 'conflict-branch' conflicts with main
    jjq: rebase onto main and resolve conflicts before pushing
    jjq: revision conflicts with trunk
    ");
}

#[test]
fn test_run_empty_queue() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    let output = repo.jjq_success(&["run"]);
    insta::assert_snapshot!(output, @"jjq: queue is empty");
}

#[test]
fn test_run_no_check_command() {
    let repo = TestRepo::with_go_project();

    // Push something first
    run_jj(repo.path(), &["new", "-m", "test", "main"]);
    fs::write(repo.path().join("test.txt"), "test").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "test-branch"]);
    repo.jjq_success(&["push", "test-branch"]);

    // Run without check_command configured
    let output = repo.jjq_failure(&["run"]);
    insta::assert_snapshot!(output, @r"
    jjq: processing queue item 1
    jjq: check_command not configured (use 'jjq config check_command <cmd>')
    jjq: check_command not configured
    ");
}

#[test]
fn test_run_success() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    // Create and push a simple branch
    run_jj(repo.path(), &["new", "-m", "add file", "main"]);
    fs::write(repo.path().join("newfile.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "simple-branch"]);
    repo.jjq_success(&["push", "simple-branch"]);

    let output = repo.jjq_success(&["run"]);
    insta::assert_snapshot!(output, @r"
    jjq: processing queue item 1
    jjq: merged 1 to main (now at <CHANGE_ID>)
    ");
}

#[test]
fn test_run_check_failure() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    // Create and push a branch
    run_jj(repo.path(), &["new", "-m", "will fail check", "main"]);
    fs::write(repo.path().join("file.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);

    let run_output = repo.jjq_failure(&["run"]);
    insta::assert_snapshot!(run_output, @r"
    jjq: processing queue item 1


    jjq: merge 1 check failed
    jjq: workspace remains: <TEMP_PATH>
    jjq: merge 1 check failed
    ");

    let status_output = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_output, @r"
    jjq: Failed (recent):
      1: <CHANGE_ID> Failed: merge 1 (check)
    ");
}

#[test]
fn test_run_all() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "true"]);

    // Create multiple branches
    run_jj(repo.path(), &["new", "-m", "feature 1", "main"]);
    fs::write(repo.path().join("f1.txt"), "feature 1").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);

    run_jj(repo.path(), &["new", "-m", "feature 2", "main"]);
    fs::write(repo.path().join("f2.txt"), "feature 2").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f2"]);

    run_jj(repo.path(), &["new", "-m", "feature 3", "main"]);
    fs::write(repo.path().join("f3.txt"), "feature 3").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f3"]);

    // Push all
    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    let status_before = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_before, @r"
    jjq: Queued:
      1: <CHANGE_ID> feature 1
      2: <CHANGE_ID> feature 2
      3: <CHANGE_ID> feature 3
    ");

    let run_output = repo.jjq_success(&["run", "--all"]);
    insta::assert_snapshot!(run_output, @r"
    jjq: processing queue item 1
    jjq: merged 1 to main (now at <CHANGE_ID>)
    jjq: processing queue item 2
    jjq: merged 2 to main (now at <CHANGE_ID>)
    jjq: processing queue item 3
    jjq: merged 3 to main (now at <CHANGE_ID>)
    jjq: queue is empty
    jjq: processed 3 item(s)
    ");

    let status_after = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after, @"jjq: queue is empty");
}

#[test]
fn test_delete_queued() {
    let repo = TestRepo::with_go_project();

    run_jj(repo.path(), &["new", "-m", "to delete", "main"]);
    fs::write(repo.path().join("delete.txt"), "delete me").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "delete-branch"]);
    repo.jjq_success(&["push", "delete-branch"]);

    let status_before = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_before, @r"
    jjq: Queued:
      1: <CHANGE_ID> to delete
    ");

    let delete_output = repo.jjq_success(&["delete", "1"]);
    insta::assert_snapshot!(delete_output, @"jjq: deleted queued item 1");

    let status_after = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after, @"jjq: queue is empty");
}

#[test]
fn test_delete_failed() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("fail.txt"), "fail").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);

    // Run to create failed item
    repo.jjq_failure(&["run"]);

    let status_before = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_before, @r"
    jjq: Failed (recent):
      1: <CHANGE_ID> Failed: merge 1 (check)
    ");

    let delete_output = repo.jjq_success(&["delete", "1"]);
    insta::assert_snapshot!(delete_output, @"jjq: deleted failed item 1");

    let status_after = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after, @"jjq: queue is empty");
}

#[test]
fn test_delete_not_found() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["push", "main"]); // Initialize jjq
    repo.jjq_success(&["delete", "1"]); // Delete the item we just pushed

    let output = repo.jjq_failure(&["delete", "999"]);
    insta::assert_snapshot!(output, @"jjq: item 999 not found in queue or failed");
}

#[test]
fn test_retry_basic() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    run_jj(repo.path(), &["new", "-m", "retry me", "main"]);
    fs::write(repo.path().join("retry.txt"), "retry").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "retry-branch"]);
    repo.jjq_success(&["push", "retry-branch"]);

    // Run to create failed item
    repo.jjq_failure(&["run"]);

    let status_before = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_before, @r"
    jjq: Failed (recent):
      1: <CHANGE_ID> Failed: merge 1 (check)
    ");

    let retry_output = repo.jjq_success(&["retry", "1"]);
    insta::assert_snapshot!(retry_output, @r"
    jjq: retrying failed item 1 using original candidate <CHANGE_ID>
    jjq: revision queued at 2
    ");

    let status_after = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after, @r"
    jjq: Queued:
      2: <CHANGE_ID> retry me
    ");
}

#[test]
fn test_retry_with_revset() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    run_jj(repo.path(), &["new", "-m", "original", "main"]);
    fs::write(repo.path().join("orig.txt"), "original").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "orig-branch"]);
    repo.jjq_success(&["push", "orig-branch"]);

    // Create alternate revision
    run_jj(repo.path(), &["new", "-m", "alternate", "main"]);
    fs::write(repo.path().join("alt.txt"), "alternate").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "alt-branch"]);

    // Run to create failed item
    repo.jjq_failure(&["run"]);

    let retry_output = repo.jjq_success(&["retry", "1", "alt-branch"]);
    insta::assert_snapshot!(retry_output, @r"
    jjq: retrying failed item 1 using 'alt-branch'
    jjq: revision queued at 2
    ");

    let status_output = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_output, @r"
    jjq: Queued:
      2: <CHANGE_ID> alternate
    ");
}

#[test]
fn test_retry_not_found() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["push", "main"]); // Initialize

    let output = repo.jjq_failure(&["retry", "999"]);
    insta::assert_snapshot!(output, @"jjq: failed item 999 not found");
}

#[test]
fn test_sequence_id_validation() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["push", "main"]); // Initialize

    // Test invalid IDs
    let empty = repo.jjq_failure(&["delete", ""]);
    insta::assert_snapshot!(empty, @"jjq: invalid sequence ID: empty");

    let negative = repo.jjq_failure(&["delete", "-1"]);
    insta::assert_snapshot!(negative, @r"
    error: unexpected argument '-1' found

      tip: to pass '-1' as a value, use '-- -1'

    Usage: jjq delete <ID>

    For more information, try '--help'.
    ");

    let non_numeric = repo.jjq_failure(&["delete", "abc"]);
    insta::assert_snapshot!(non_numeric, @"jjq: invalid sequence ID: 'abc' (must be numeric)");

    let too_large = repo.jjq_failure(&["delete", "1000000"]);
    insta::assert_snapshot!(too_large, @"jjq: invalid sequence ID: 1000000 (must be 1-999999)");

    let zero = repo.jjq_failure(&["delete", "0"]);
    insta::assert_snapshot!(zero, @"jjq: invalid sequence ID: 0 (must be 1-999999)");

    // Padded forms should work
    let padded = repo.jjq_success(&["delete", "000001"]);
    insta::assert_snapshot!(padded, @"jjq: deleted queued item 1");
}

#[test]
fn test_full_workflow_with_prs() {
    let repo = TestRepo::with_prs();
    repo.jjq_success(&["config", "check_command", "make"]);

    // Push all PRs
    let push1 = repo.jjq_success(&["push", "pr1"]);
    insta::assert_snapshot!(push1, @"jjq: revision 'pr1' queued at 1");

    let push2 = repo.jjq_success(&["push", "pr2"]);
    insta::assert_snapshot!(push2, @"jjq: revision 'pr2' queued at 2");

    let push3 = repo.jjq_success(&["push", "pr3"]);
    insta::assert_snapshot!(push3, @"jjq: revision 'pr3' queued at 3");

    let push4 = repo.jjq_success(&["push", "pr4"]);
    insta::assert_snapshot!(push4, @"jjq: revision 'pr4' queued at 4");

    let status = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status, @r"
    jjq: Queued:
      1: <CHANGE_ID> add greeting pkg
      2: <CHANGE_ID> add goodbye
      3: <CHANGE_ID> add comment
      4: <CHANGE_ID> add readme
    ");

    // Run pr1 - should succeed
    let run1 = repo.jjq_success(&["run"]);
    insta::assert_snapshot!(run1, @r"
    jjq: processing queue item 1
    jjq: merged 1 to main (now at <CHANGE_ID>)
    ");

    // Run pr2 - will have conflicts since pr1 changed main.go
    let run2 = repo.jjq_output(&["run"]);
    insta::assert_snapshot!(run2, @r"
    jjq: processing queue item 2
    jjq: merge 2 has conflicts, marked as failed
    jjq: workspace remains: <TEMP_PATH>
    jjq: merge 2 has conflicts
    ");

    // Check status shows failed item
    let status_after_conflict = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after_conflict, @r"
    jjq: Queued:
      3: <CHANGE_ID> add comment
      4: <CHANGE_ID> add readme

    jjq: Failed (recent):
      2: <CHANGE_ID> Failed: merge 2 (conflicts)
    ");
}

#[test]
fn test_multiple_push_same_revision() {
    let repo = TestRepo::with_go_project();

    // Push main twice - should get different sequence IDs
    let push1 = repo.jjq_success(&["push", "main"]);
    insta::assert_snapshot!(push1, @"jjq: revision 'main' queued at 1");

    let push2 = repo.jjq_success(&["push", "main"]);
    insta::assert_snapshot!(push2, @"jjq: revision 'main' queued at 2");

    let status = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status, @r"
    jjq: Queued:
      1: <CHANGE_ID> initial
      2: <CHANGE_ID> initial
    ");
}

#[test]
fn test_log_hint_not_shown_in_non_tty() {
    let repo = TestRepo::with_go_project();

    // Push should succeed without showing hint (non-TTY mode)
    let output = repo.jjq_success(&["push", "main"]);

    // Verify no hint in output
    assert!(
        !output.contains("hint:"),
        "hint should not appear in non-TTY mode"
    );
    assert!(
        !output.contains("jj config set"),
        "config suggestion should not appear in non-TTY mode"
    );

    // Verify log_hint_shown was NOT recorded (non-TTY returns early)
    assert!(
        !repo.jj_file_exists("log_hint_shown", "jjq/_/_"),
        "log_hint_shown should not be recorded in non-TTY mode"
    );
}

#[test]
fn test_log_hint_shown_once_when_forced() {
    let repo = TestRepo::with_go_project();

    // First push with forced hint - should show hint
    let output1 = repo.jjq_with_env(&["push", "main"], &[("JJQTEST_FORCE_HINT", "1")]);
    assert!(
        output1.contains("hint:"),
        "hint should appear on first push when forced"
    );
    assert!(
        output1.contains("jj config set"),
        "config suggestion should appear"
    );

    // Verify log_hint_shown was recorded
    assert!(
        repo.jj_file_exists("log_hint_shown", "jjq/_/_"),
        "log_hint_shown should be recorded after hint shown"
    );

    // Second push with forced hint - should NOT show hint again
    let output2 = repo.jjq_with_env(&["push", "main"], &[("JJQTEST_FORCE_HINT", "1")]);
    assert!(
        !output2.contains("hint:"),
        "hint should not appear on second push"
    );
}

#[test]
fn test_log_hint_skipped_when_filter_configured() {
    let repo = TestRepo::with_go_project();

    // Configure the log filter first
    run_jj(repo.path(), &["config", "set", "--repo", "revsets.log", "~ ::jjq/_/_"]);

    // Push with forced hint - should skip because filter already configured
    let output = repo.jjq_with_env(&["push", "main"], &[("JJQTEST_FORCE_HINT", "1")]);
    assert!(
        !output.contains("hint:"),
        "hint should not appear when log filter already configured"
    );

    // Verify log_hint_shown was NOT recorded (skipped due to filter)
    assert!(
        !repo.jj_file_exists("log_hint_shown", "jjq/_/_"),
        "log_hint_shown should not be recorded when filter already configured"
    );
}
