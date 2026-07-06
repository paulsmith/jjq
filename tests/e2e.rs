// ABOUTME: End-to-end tests for jjq using insta snapshot testing.
// ABOUTME: Tests the full merge queue workflow including conflict handling.

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::LazyLock;
use tempfile::TempDir;

static JJQ_BIN: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var_os("CARGO_BIN_EXE_jjq")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[allow(deprecated)]
            assert_cmd::cargo::cargo_bin("jjq")
        })
});

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

    /// Create a test repository with main branch, copied from a cached template.
    fn with_go_project() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().to_path_buf();
        copy_dir_all(&GO_PROJECT_TEMPLATE.path, &path)
            .expect("failed to copy go project template");
        TestRepo { dir, path }
    }

    /// Create a full test repo with all 4 PRs, copied from a cached template.
    fn with_prs() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().to_path_buf();
        copy_dir_all(&PRS_TEMPLATE.path, &path)
            .expect("failed to copy prs template");
        TestRepo { dir, path }
    }

    /// Create a repo with 3 non-conflicting branches (f1/f2/f3) for run --all success tests.
    fn with_run_all_happy_scenario() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().to_path_buf();
        copy_dir_all(&RUN_ALL_HAPPY_TEMPLATE.path, &path)
            .expect("failed to copy run-all happy template");
        TestRepo { dir, path }
    }

    /// Create a repo with f1/f2/f3 where f2 conflicts after f1, and f3 is clean.
    fn with_run_all_conflict_scenario() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().to_path_buf();
        copy_dir_all(&RUN_ALL_CONFLICT_TEMPLATE.path, &path)
            .expect("failed to copy run-all conflict template");
        TestRepo { dir, path }
    }

    /// Create a repo with f1 clean, f2/f3 conflicting, for partial failure exit-code tests.
    fn with_run_all_partial_scenario() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().to_path_buf();
        copy_dir_all(&RUN_ALL_PARTIAL_TEMPLATE.path, &path)
            .expect("failed to copy run-all partial template");
        TestRepo { dir, path }
    }

    /// Base jjq invocation as a std::process::Command with the shared test
    /// environment applied. Helpers that need process-level control
    /// (timeouts, background children) build on this; `jjq()` wraps it for
    /// assert_cmd assertions.
    fn jjq_std(&self) -> process::Command {
        let mut cmd = process::Command::new(&*JJQ_BIN);
        cmd.current_dir(&self.path);
        cmd.env("NON_INTERACTIVE", "1");
        // Speed up jj subprocess calls within jjq
        cmd.env("JJ_OP_HOSTNAME", "test");
        cmd.env("JJ_OP_USERNAME", "test");
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("HOME", &self.path);
        cmd
    }

    /// Get a jjq Command configured for this repo.
    fn jjq(&self) -> Command {
        Command::from_std(self.jjq_std())
    }

    /// Get a jjq Command that runs from the given directory (e.g. a secondary
    /// workspace) with the same environment as `jjq()`.
    fn jjq_in(&self, dir: &Path) -> Command {
        let mut cmd = self.jjq();
        cmd.current_dir(dir);
        cmd
    }

    /// Run jjq and return normalized output for snapshots.
    fn jjq_output(&self, args: &[&str]) -> String {
        let output = self.jjq().args(args).output().expect("failed to run jjq");

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
            .env("JJ_OP_HOSTNAME", "test")
            .env("JJ_OP_USERNAME", "test")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &self.path)
            .output()
            .expect("failed to run jj");
        output.status.success()
    }

    /// Run jjq and return raw stdout/stderr without normalization.
    fn jjq_raw_output(&self, args: &[&str]) -> (String, String, bool) {
        let output = self.jjq().args(args).output().expect("failed to run jjq");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        (stdout, stderr, output.status.success())
    }

    /// Run jjq with a wall-clock timeout, killing the process if it exceeds
    /// it. Returns (stdout, stderr, exit_code); exit_code is None if killed.
    /// Guards tests where a regression would make jjq loop indefinitely.
    fn jjq_with_timeout(
        &self,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> (String, String, Option<i32>) {
        use std::io::Read;

        let mut child = self
            .jjq_std()
            .args(args)
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped())
            .spawn()
            .expect("failed to spawn jjq");

        // Drain pipes on threads so the child can't block on a full pipe buffer
        let mut stdout_pipe = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        let stdout_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stdout_pipe.read_to_string(&mut buf);
            buf
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr_pipe.read_to_string(&mut buf);
            buf
        });

        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().expect("failed to poll jjq") {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        let stdout = stdout_thread.join().expect("stdout reader panicked");
        let stderr = stderr_thread.join().expect("stderr reader panicked");
        (stdout, stderr, status.and_then(|s| s.code()))
    }

    /// Run jjq with additional environment variables.
    fn jjq_with_env(&self, args: &[&str], env_vars: &[(&str, &str)]) -> String {
        let mut cmd = self.jjq();
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        let output = cmd.args(args).output().expect("failed to run jjq");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let combined = format!("{}{}", stdout, stderr);
        normalize_output(&combined, &self.path)
    }

    /// Spawn jjq as a background child process with the standard test
    /// environment, stdout/stderr piped. Callers wait via wait_with_output.
    fn spawn_jjq(&self, args: &[&str]) -> process::Child {
        self.jjq_std()
            .args(args)
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped())
            .spawn()
            .expect("failed to spawn jjq")
    }

    /// Initialize jjq with default settings (check command = "true").
    fn init_jjq(&self) {
        self.jjq_success(&["init", "--trunk", "main", "--check", "true"]);
    }

    /// Initialize jjq with a specific check command.
    fn init_jjq_with_check(&self, check_cmd: &str) {
        self.jjq_success(&["init", "--trunk", "main", "--check", check_cmd]);
    }

    /// Initialize jjq with merge strategy and default settings (check command = "true").
    fn init_jjq_merge(&self) {
        self.jjq_success(&["init", "--trunk", "main", "--check", "true", "--strategy", "merge"]);
    }

    /// Initialize jjq with merge strategy and a specific check command.
    fn init_jjq_merge_with_check(&self, check_cmd: &str) {
        self.jjq_success(&["init", "--trunk", "main", "--check", check_cmd, "--strategy", "merge"]);
    }
}

/// Run a jj command in the given directory.
fn run_jj(dir: &Path, args: &[&str]) -> String {
    let output = process::Command::new("jj")
        .current_dir(dir)
        .args(args)
        .env("JJ_OP_HOSTNAME", "test")
        .env("JJ_OP_USERNAME", "test")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("failed to run jj");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("jj {:?} failed: {}", args, stderr);
    }

    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Recursively copy a directory tree.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Pre-built repository template, created once and copied for each test.
struct RepoTemplate {
    _dir: TempDir,
    path: PathBuf,
}

/// Cached template: jj repo with a Go project (main branch with initial commit).
/// All Go code is pre-formatted so no `go fmt` call is needed.
static GO_PROJECT_TEMPLATE: LazyLock<RepoTemplate> = LazyLock::new(|| {
    let dir = TempDir::new().expect("failed to create template dir");
    let path = dir.path().to_path_buf();

    run_jj(&path, &["git", "init", "."]);

    fs::write(
        path.join("go.mod"),
        "module example/jjdemo\n\ngo 1.24\n",
    )
    .unwrap();

    fs::write(
        path.join("main.go"),
        r#"package main

import "fmt"

func main() {
	fmt.Println("Hello, world!")
}
"#,
    )
    .unwrap();

    fs::write(
        path.join("main_test.go"),
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

    fs::write(
        path.join("Makefile"),
        "all: test\ntest:\n\tgo test -v ./...\n",
    )
    .unwrap();

    run_jj(&path, &["desc", "-m", "initial"]);
    run_jj(&path, &["bookmark", "create", "main"]);

    RepoTemplate { _dir: dir, path }
});

/// Cached template: Go project with 4 PRs branching off main.
static PRS_TEMPLATE: LazyLock<RepoTemplate> = LazyLock::new(|| {
    let dir = TempDir::new().expect("failed to create prs template dir");
    let path = dir.path().to_path_buf();
    copy_dir_all(&GO_PROJECT_TEMPLATE.path, &path)
        .expect("failed to copy go project template for prs");

    // PR1: Add greeting package
    run_jj(&path, &["new", "-m", "add greeting pkg", "main"]);
    fs::create_dir_all(path.join("say")).unwrap();
    fs::write(
        path.join("say/greet.go"),
        r#"package say

func Greet(name string) string {
	return "Hello, " + name + "!"
}
"#,
    )
    .unwrap();
    fs::write(
        path.join("main.go"),
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
    run_jj(&path, &["bookmark", "create", "pr1"]);

    // PR2: Add goodbye
    run_jj(&path, &["new", "-m", "add goodbye", "main"]);
    fs::create_dir_all(path.join("say")).unwrap();
    fs::write(
        path.join("say/bye.go"),
        r#"package say

func Bye() string {
	return "Goodbye."
}
"#,
    )
    .unwrap();
    fs::write(
        path.join("main.go"),
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
        path.join("main_test.go"),
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
    run_jj(&path, &["bookmark", "create", "pr2"]);

    // PR3: Add comment
    run_jj(&path, &["new", "-m", "add comment", "main"]);
    fs::write(
        path.join("main.go"),
        r#"package main

import "fmt"

func main() {
	// say hi
	fmt.Println("Hello, world!")
}
"#,
    )
    .unwrap();
    run_jj(&path, &["bookmark", "create", "pr3"]);

    // PR4: Add readme
    run_jj(&path, &["new", "-m", "add readme", "main"]);
    fs::write(path.join("README.md"), "# jjq demo\n").unwrap();
    run_jj(&path, &["bookmark", "create", "pr4"]);

    // Return to main
    run_jj(&path, &["new", "main"]);

    RepoTemplate { _dir: dir, path }
});

/// Cached template: 3 non-conflicting branches off main for run --all happy path.
static RUN_ALL_HAPPY_TEMPLATE: LazyLock<RepoTemplate> = LazyLock::new(|| {
    let dir = TempDir::new().expect("failed to create run-all happy template dir");
    let path = dir.path().to_path_buf();
    copy_dir_all(&GO_PROJECT_TEMPLATE.path, &path)
        .expect("failed to copy go project template for run-all happy template");

    run_jj(&path, &["new", "-m", "feature 1", "main"]);
    fs::write(path.join("f1.txt"), "feature 1").unwrap();
    run_jj(&path, &["bookmark", "create", "f1"]);

    run_jj(&path, &["new", "-m", "feature 2", "main"]);
    fs::write(path.join("f2.txt"), "feature 2").unwrap();
    run_jj(&path, &["bookmark", "create", "f2"]);

    run_jj(&path, &["new", "-m", "feature 3", "main"]);
    fs::write(path.join("f3.txt"), "feature 3").unwrap();
    run_jj(&path, &["bookmark", "create", "f3"]);

    run_jj(&path, &["new", "main"]);

    RepoTemplate { _dir: dir, path }
});

/// Cached template: f2 conflicts after f1 lands; f3 merges cleanly.
static RUN_ALL_CONFLICT_TEMPLATE: LazyLock<RepoTemplate> = LazyLock::new(|| {
    let dir = TempDir::new().expect("failed to create run-all conflict template dir");
    let path = dir.path().to_path_buf();
    copy_dir_all(&GO_PROJECT_TEMPLATE.path, &path)
        .expect("failed to copy go project template for run-all conflict template");

    run_jj(&path, &["new", "-m", "feature 1", "main"]);
    fs::write(
        path.join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 1\")\n}\n",
    )
    .unwrap();
    run_jj(&path, &["bookmark", "create", "f1"]);

    run_jj(&path, &["new", "-m", "feature 2", "main"]);
    fs::write(
        path.join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 2\")\n}\n",
    )
    .unwrap();
    run_jj(&path, &["bookmark", "create", "f2"]);

    run_jj(&path, &["new", "-m", "feature 3", "main"]);
    fs::write(path.join("f3.txt"), "feature 3").unwrap();
    run_jj(&path, &["bookmark", "create", "f3"]);

    run_jj(&path, &["new", "main"]);

    RepoTemplate { _dir: dir, path }
});

/// Cached template: f1 is clean; f2/f3 conflict with each other after f1 lands.
static RUN_ALL_PARTIAL_TEMPLATE: LazyLock<RepoTemplate> = LazyLock::new(|| {
    let dir = TempDir::new().expect("failed to create run-all partial template dir");
    let path = dir.path().to_path_buf();
    copy_dir_all(&GO_PROJECT_TEMPLATE.path, &path)
        .expect("failed to copy go project template for run-all partial template");

    run_jj(&path, &["new", "-m", "feature 1", "main"]);
    fs::write(path.join("f1.txt"), "feature 1").unwrap();
    run_jj(&path, &["bookmark", "create", "f1"]);

    run_jj(&path, &["new", "-m", "feature 2", "main"]);
    fs::write(
        path.join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 2\")\n}\n",
    )
    .unwrap();
    run_jj(&path, &["bookmark", "create", "f2"]);

    run_jj(&path, &["new", "-m", "feature 3", "main"]);
    fs::write(
        path.join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"feature 3\")\n}\n",
    )
    .unwrap();
    run_jj(&path, &["bookmark", "create", "f3"]);

    run_jj(&path, &["new", "main"]);

    RepoTemplate { _dir: dir, path }
});

/// Normalize output for snapshot comparison.
/// Replaces change IDs, commit IDs, paths, and other variable content.
fn normalize_output(output: &str, repo_path: &Path) -> String {
    static RE_TEMP: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"/var/folders/[^\s]+|/tmp/[^\s]+|/private/var/folders/[^\s]+").unwrap()
    });
    static RE_CHANGE_ID: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\b[a-z]{12}\b").unwrap()
    });
    static RE_NOW_AT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\(now at [a-z]+\)").unwrap()
    });

    let mut result = output.to_string();

    // Replace the repo path with a placeholder (canonical path first to handle
    // macOS /private prefix, then the original path)
    if let Ok(canonical) = repo_path.canonicalize() {
        let canonical_str = canonical.to_string_lossy();
        result = result.replace(&*canonical_str, "<REPO>");
    }
    let repo_str = repo_path.to_string_lossy();
    result = result.replace(&*repo_str, "<REPO>");

    // Replace temp directory paths (various formats)
    result = RE_TEMP.replace_all(&result, "<TEMP_PATH>").to_string();

    // Replace change IDs (12 lowercase letters)
    result = RE_CHANGE_ID.replace_all(&result, "<CHANGE_ID>").to_string();

    // Replace short change IDs in "now at <id>" pattern
    result = RE_NOW_AT
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
    insta::assert_snapshot!(output, @"jjq: jjq not initialized. Run 'jjq init' first.");
}

#[test]
fn test_init_no_trunk_bookmark() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("file.txt"), "content").unwrap();
    run_jj(repo.path(), &["desc", "-m", "test commit"]);
    // Init with trunk "main" which doesn't exist — should fail in non-interactive mode
    let output = repo.jjq_failure(&["init", "--trunk", "main", "--check", "true"]);
    insta::assert_snapshot!(output, @r"
    Initializing jjq in this repository.

    jjq: trunk bookmark 'main' does not exist.
    ");
}

#[test]
fn test_config_show_all() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();
    let output = repo.jjq_success(&["config"]);
    insta::assert_snapshot!(output, @r"
    trunk_bookmark = main
    check_command = true
    strategy = rebase
    ");
}

#[test]
fn test_config_set_and_get() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let set_output = repo.jjq_success(&["config", "check_command", "make test"]);
    insta::assert_snapshot!(set_output, @"jjq: check_command = make test");

    let get_output = repo.jjq_success(&["config", "check_command"]);
    insta::assert_snapshot!(get_output, @"make test");
}

#[test]
fn test_config_invalid_key() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let output = repo.jjq_failure(&["config", "invalid_key"]);
    insta::assert_snapshot!(output, @r"
    jjq: unknown config key: invalid_key
    valid keys: trunk_bookmark, check_command, strategy
    ");
}

#[test]
fn test_init_sets_log_filter() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // init should automatically configure jj to hide jjq metadata, using the
    // bookmarks(exact:) form that stays harmless if the bookmark conflicts
    let config_output = run_jj(repo.path(), &["config", "get", "revsets.log"]);
    assert!(
        config_output.contains("bookmarks(exact:\"jjq/_/_\")"),
        "init should set revsets.log to hide jjq metadata via bookmarks(exact:): {}",
        config_output
    );
}

#[test]
fn test_init_log_filter_preserves_existing() {
    let repo = TestRepo::with_go_project();

    // Set a custom revsets.log before init
    run_jj(
        repo.path(),
        &["config", "set", "--repo", "revsets.log", "all()"],
    );

    repo.init_jjq();

    let config_output = run_jj(repo.path(), &["config", "get", "revsets.log"]);
    assert!(
        config_output.contains("(all()) ~ ::bookmarks(exact:\"jjq/_/_\")"),
        "init should wrap existing filter and append exclusion: {}",
        config_output
    );
}

#[test]
fn test_push_and_status() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a branch to push
    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);

    let push_output = repo.jjq_success(&["push", "feature"]);
    insta::assert_snapshot!(push_output, @r"
    jjq: revision 'feature' queued at 1 (trunk: main in <REPO>)
    jjq: working copy moved to a new change; the queued commit stays as pushed
    ");

    let status_output = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_output, @r"
    jjq: Queued:
      1: <CHANGE_ID> test feature
    ");
}

#[test]
fn test_push_conflict_detection() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a branch that modifies main.go one way
    run_jj(repo.path(), &["new", "-m", "trunk advance", "main"]);
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
    run_jj(repo.path(), &["bookmark", "move", "main"]);

    // Create a conflicting branch from the original main (root)
    run_jj(repo.path(), &["new", "-m", "conflicting change", "root()"]);
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
    repo.init_jjq();

    let output = repo.jjq_success(&["run"]);
    insta::assert_snapshot!(output, @"jjq: queue is empty");
}

#[test]
fn test_run_without_init() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_failure(&["run"]);
    insta::assert_snapshot!(output, @"jjq: jjq is not initialized. Run 'jjq init' first.");
}

#[test]
fn test_run_success() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge();

    // Create and push a simple branch
    run_jj(repo.path(), &["new", "-m", "add file", "main"]);
    fs::write(repo.path().join("newfile.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "simple-branch"]);
    repo.jjq_success(&["push", "simple-branch"]);

    let output = repo.jjq_success(&["run"]);
    insta::assert_snapshot!(output, @r"
    jjq: processing queue item 1 (merge strategy)
    jjq: merged 1 to main (now at <CHANGE_ID>)
    ");
}

#[test]
fn test_run_check_failure() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge_with_check("false");

    // Create and push a branch
    run_jj(repo.path(), &["new", "-m", "will fail check", "main"]);
    fs::write(repo.path().join("file.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);

    let run_output = repo.jjq_failure(&["run"]);
    insta::assert_snapshot!(run_output, @r"
    jjq: processing queue item 1 (merge strategy)
    jjq: merge 1 failed check, marked as failed
    jjq:   candidate: <CHANGE_ID>
    jjq:
    jjq: To resolve:
    jjq:   # fix the issue in <CHANGE_ID>
    jjq:   jjq push <CHANGE_ID>
    ");

    let status_output = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_output, @r"
    jjq: Failed (recent):
      1: <CHANGE_ID> will fail check
    ");
}

#[test]
fn test_run_all() {
    let repo = TestRepo::with_run_all_happy_scenario();
    repo.init_jjq_merge();

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
    jjq: processing queue item 1 (merge strategy)
    jjq: merged 1 to main (now at <CHANGE_ID>)
    jjq: processing queue item 2 (merge strategy)
    jjq: merged 2 to main (now at <CHANGE_ID>)
    jjq: processing queue item 3 (merge strategy)
    jjq: merged 3 to main (now at <CHANGE_ID>)
    jjq: queue is empty
    jjq: processed 3 item(s)
    ");

    let status_after = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after, @r"
    jjq: Landed (recent):
      3: <CHANGE_ID> Success: merge 3
      2: <CHANGE_ID> Success: merge 2
      1: <CHANGE_ID> Success: merge 1
    ");
}

#[test]
fn test_delete_queued() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

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
    repo.init_jjq_with_check("false");

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("fail.txt"), "fail").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);

    // Run to create failed item
    repo.jjq_failure(&["run"]);

    let status_before = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_before, @r"
    jjq: Failed (recent):
      1: <CHANGE_ID> will fail
    ");

    let delete_output = repo.jjq_success(&["delete", "1"]);
    insta::assert_snapshot!(delete_output, @r"
    jjq: deleted failed item 1
    jjq: removed workspace <TEMP_PATH>
    ");

    let status_after = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after, @"jjq: queue is empty");
}

#[test]
fn test_delete_not_found() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let output = repo.jjq_failure(&["delete", "999"]);
    insta::assert_snapshot!(output, @"jjq: item 999 not found in queue or failed");
}

#[test]
fn test_sequence_id_validation() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();
    repo.jjq_success(&["push", "main"]); // Need an item to delete

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
    // This test validates queue/conflict behavior; an expensive project build
    // check is unnecessary here.
    repo.init_jjq_merge_with_check("true");

    // Push all PRs
    let push1 = repo.jjq_success(&["push", "pr1"]);
    insta::assert_snapshot!(push1, @"jjq: revision 'pr1' queued at 1 (trunk: main in <REPO>)");

    let push2 = repo.jjq_success(&["push", "pr2"]);
    insta::assert_snapshot!(push2, @"jjq: revision 'pr2' queued at 2 (trunk: main in <REPO>)");

    let push3 = repo.jjq_success(&["push", "pr3"]);
    insta::assert_snapshot!(push3, @"jjq: revision 'pr3' queued at 3 (trunk: main in <REPO>)");

    let push4 = repo.jjq_success(&["push", "pr4"]);
    insta::assert_snapshot!(push4, @"jjq: revision 'pr4' queued at 4 (trunk: main in <REPO>)");

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
    jjq: processing queue item 1 (merge strategy)
    jjq: merged 1 to main (now at <CHANGE_ID>)
    ");

    // Run pr2 - will have conflicts since pr1 changed main.go
    let run2 = repo.jjq_output(&["run"]);
    insta::assert_snapshot!(run2, @r"
    jjq: processing queue item 2 (merge strategy)
    jjq: merge 2 has conflicts, marked as failed
    jjq:   candidate: <CHANGE_ID>
    jjq:   conflicting files: main.go
    jjq:
    jjq: To resolve:
    jjq:   jj rebase -r <CHANGE_ID> -d main
    jjq:   # resolve conflicts in <CHANGE_ID>
    jjq:   jjq push <CHANGE_ID>
    ");

    // Check status shows failed item and landed item
    let status_after_conflict = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status_after_conflict, @r"
    jjq: Queued:
      3: <CHANGE_ID> add comment
      4: <CHANGE_ID> add readme

    jjq: Failed (recent):
      2: <CHANGE_ID> add goodbye (conflicts: main.go)

    jjq: Landed (recent):
      1: <CHANGE_ID> Success: merge 1
    ");
}

#[test]
fn test_multiple_push_same_revision() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Push main once - should succeed. The template's working copy sits on
    // the main commit itself, so the push also parks the working copy on a
    // fresh child.
    let push1 = repo.jjq_success(&["push", "main"]);
    insta::assert_snapshot!(push1, @r"
    jjq: revision 'main' queued at 1 (trunk: main in <REPO>)
    jjq: working copy moved to a new change; the queued commit stays as pushed
    ");

    // Push same commit ID again - should be rejected as duplicate
    let push2 = repo.jjq_output(&["push", "main"]);
    assert!(
        push2.contains("already queued at 1"),
        "should reject duplicate: {}",
        push2
    );
}

#[test]
fn test_log_hint_not_shown_in_non_tty() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

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
    repo.init_jjq();

    // Remove log filter to simulate a repo initialized before auto-setup
    run_jj(repo.path(), &["config", "unset", "--repo", "revsets.log"]);

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
fn test_run_all_stop_on_failure_flag() {
    let repo = TestRepo::with_run_all_conflict_scenario();
    repo.init_jjq_merge();

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // run --all should process f1, fail on f2 (conflict), and STOP (not process f3)
    let output = repo.jjq_output(&["run", "--all", "--stop-on-failure"]);
    assert!(
        output.contains("rebased 1 to main") || output.contains("merged 1 to main"),
        "f1 should land: {}",
        output
    );
    assert!(
        output.contains("merge 2 has conflicts"),
        "f2 should conflict: {}",
        output
    );
    assert!(
        !output.contains("rebased 3 to main") && !output.contains("merged 3 to main"),
        "f3 should NOT be processed: {}",
        output
    );
    assert!(
        output.contains("processed 1 item(s) before failure"),
        "summary should show 1 processed before failure: {}",
        output
    );

    // f3 should still be in the queue
    let status = repo.jjq_success(&["status"]);
    assert!(
        status.contains("feature 3"),
        "f3 should still be queued: {}",
        status
    );
}

#[test]
fn test_run_all_continues_on_failure() {
    let repo = TestRepo::with_run_all_conflict_scenario();
    repo.init_jjq_merge();

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // run --all should process f1, fail on f2 (conflict), CONTINUE, and process f3
    let output = repo.jjq_output(&["run", "--all"]);
    assert!(
        output.contains("rebased 1 to main") || output.contains("merged 1 to main"),
        "f1 should land: {}",
        output
    );
    assert!(
        output.contains("merge 2 has conflicts"),
        "f2 should conflict: {}",
        output
    );
    assert!(
        output.contains("rebased 3 to main") || output.contains("merged 3 to main"),
        "f3 SHOULD be processed: {}",
        output
    );
    assert!(
        output.contains("processed 2 item(s), 1 failed"),
        "summary should show mixed results: {}",
        output
    );
}

#[test]
fn test_run_all_partial_failure_exit_code() {
    let repo = TestRepo::with_run_all_partial_scenario();
    repo.init_jjq_merge();

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // Exit code should be 2 (PARTIAL) — some succeeded, some failed
    repo.jjq().args(["run", "--all"]).assert().code(2);
}

#[test]
fn test_log_hint_skipped_when_filter_configured() {
    let repo = TestRepo::with_go_project();

    // Configure the log filter first
    run_jj(
        repo.path(),
        &["config", "set", "--repo", "revsets.log", "~ ::jjq/_/_"],
    );

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

#[test]
fn test_push_exact_duplicate_rejected() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    repo.jjq_success(&["push", "main"]);

    // Same commit ID should be rejected
    let output = repo.jjq_output(&["push", "main"]);
    assert!(
        output.contains("already queued"),
        "should reject exact duplicate: {}",
        output
    );
}

#[test]
fn test_push_idempotent_clears_failed() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge_with_check("false");

    // Create and push a branch
    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("fail.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    // Run to create failed item
    repo.jjq_failure(&["run"]);

    // Verify failed item exists
    let status = repo.jjq_success(&["status"]);
    assert!(
        status.contains("Failed"),
        "should have failed item: {}",
        status
    );

    // Amend the revision (changes commit ID, keeps change ID)
    run_jj(repo.path(), &["edit", "fb"]);
    fs::write(repo.path().join("fail.txt"), "fixed content").unwrap();

    // Re-push should clear the failed entry
    let repush = repo.jjq_success(&["push", "fb"]);
    assert!(
        repush.contains("clearing failed entry"),
        "should clear failed entry: {}",
        repush
    );
}

#[test]
fn test_clean_no_workspaces() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_success(&["clean"]);
    insta::assert_snapshot!(output, @"jjq: no workspaces to clean");
}

#[test]
fn test_clean_removes_failed_workspaces() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge_with_check("false");

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("fail.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    repo.jjq_success(&["push", "fb"]);

    // Run to create failed item (and preserved workspace)
    repo.jjq_failure(&["run"]);

    // Clean should find and remove the workspace
    let output = repo.jjq_success(&["clean"]);
    assert!(
        output.contains("removed 1 workspace"),
        "should remove workspace: {}",
        output
    );
}

#[test]
fn test_status_json_empty() {
    let repo = TestRepo::new();
    let (stdout, _stderr, success) = repo.jjq_raw_output(&["status", "--json"]);
    assert!(
        success,
        "status --json should succeed on uninitialized repo"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --json should return valid JSON");
    assert_eq!(parsed["runner_active"], false);
    assert_eq!(parsed["running"], serde_json::json!([]));
    assert_eq!(parsed["queue"], serde_json::json!([]));
    assert_eq!(parsed["failed"], serde_json::json!([]));
}

#[test]
fn test_status_json_with_items() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    // Push an item and run to create a failed item
    run_jj(repo.path(), &["new", "-m", "will fail check", "main"]);
    fs::write(repo.path().join("fail.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);
    repo.jjq_failure(&["run"]);

    // Push a queued item
    run_jj(repo.path(), &["new", "-m", "queued feature", "main"]);
    fs::write(repo.path().join("queued.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "queued-branch"]);
    repo.jjq_success(&["push", "queued-branch"]);

    let (stdout, _stderr, success) = repo.jjq_raw_output(&["status", "--json"]);
    assert!(success, "status --json should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --json should return valid JSON");

    // Verify queue structure
    let queue = parsed["queue"]
        .as_array()
        .expect("queue should be an array");
    assert_eq!(queue.len(), 1, "should have 1 queued item");
    let q_item = &queue[0];
    assert!(q_item["id"].is_u64(), "queue item should have numeric id");
    assert!(
        q_item["change_id"].is_string(),
        "queue item should have change_id"
    );
    assert!(
        q_item["commit_id"].is_string(),
        "queue item should have commit_id"
    );
    assert!(
        q_item["description"].is_string(),
        "queue item should have description"
    );
    assert_eq!(q_item["description"], "queued feature");

    // Verify failed structure
    let failed = parsed["failed"]
        .as_array()
        .expect("failed should be an array");
    assert_eq!(failed.len(), 1, "should have 1 failed item");
    let f_item = &failed[0];
    assert!(f_item["id"].is_u64(), "failed item should have numeric id");
    assert!(
        f_item["candidate_change_id"].is_string(),
        "failed item should have candidate_change_id"
    );
    assert!(
        f_item["failure_reason"].is_string(),
        "failed item should have failure_reason"
    );
    assert_eq!(f_item["failure_reason"], "check");
}

#[test]
fn test_status_single_queued() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "my queued feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature-branch"]);
    repo.jjq_success(&["push", "feature-branch"]);

    let output = repo.jjq_success(&["status", "1"]);
    assert!(
        output.contains("Queue item 1"),
        "should show Queue item header: {}",
        output
    );
    assert!(
        output.contains("Change ID:"),
        "should show Change ID: {}",
        output
    );
    assert!(
        output.contains("Commit ID:"),
        "should show Commit ID: {}",
        output
    );
    assert!(
        output.contains("Description: my queued feature"),
        "should show description: {}",
        output
    );
}

#[test]
fn test_status_single_failed() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["config", "check_command", "false"]);

    run_jj(repo.path(), &["new", "-m", "will fail check", "main"]);
    fs::write(repo.path().join("fail.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);
    repo.jjq_failure(&["run"]);

    let output = repo.jjq_success(&["status", "1"]);
    assert!(
        output.contains("Failed item 1"),
        "should show Failed item header: {}",
        output
    );
    assert!(
        output.contains("Candidate:"),
        "should show Candidate: {}",
        output
    );
    assert!(
        output.contains("Description: will fail check"),
        "should show description: {}",
        output
    );
    assert!(
        output.contains("Failure:"),
        "should show Failure: {}",
        output
    );
    assert!(output.contains("Trunk:"), "should show Trunk: {}", output);
    assert!(
        output.contains("jjq push"),
        "should show concrete push command: {}",
        output
    );
    assert!(
        output.contains("To resolve:"),
        "should show resolution hints: {}",
        output
    );
}

#[test]
fn test_status_single_json() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "json feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature-branch"]);
    repo.jjq_success(&["push", "feature-branch"]);

    let (stdout, _stderr, success) = repo.jjq_raw_output(&["status", "1", "--json"]);
    assert!(success, "status 1 --json should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status 1 --json should return valid JSON");
    assert_eq!(parsed["id"], 1);
    assert!(parsed["change_id"].is_string(), "should have change_id");
    assert_eq!(parsed["description"], "json feature");
}

#[test]
fn test_status_not_found() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let output = repo.jjq_failure(&["status", "999"]);
    assert!(
        output.contains("item 999 not found"),
        "should report not found: {}",
        output
    );
}

#[test]
fn test_init_with_flags() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_success(&["init", "--trunk", "main", "--check", "make test"]);
    assert!(
        output.contains("Initialized jjq"),
        "should show init confirmation: {}",
        output
    );
    assert!(
        output.contains("trunk_bookmark = main"),
        "should show trunk: {}",
        output
    );
    assert!(
        output.contains("check_command  = make test"),
        "should show check cmd: {}",
        output
    );
    assert!(
        output.contains("Ready to go!"),
        "should show ready message: {}",
        output
    );
}

#[test]
fn test_init_already_initialized() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["init", "--trunk", "main", "--check", "true"]);

    let output = repo.jjq_failure(&["init", "--trunk", "main", "--check", "true"]);
    insta::assert_snapshot!(output, @"jjq: jjq is already initialized. Use 'jjq config' to change settings.");
}

#[test]
fn test_init_runs_doctor() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_success(&["init", "--trunk", "main", "--check", "make test"]);
    // Doctor output should be present
    assert!(
        output.contains("jj repository"),
        "should run doctor: {}",
        output
    );
    assert!(
        output.contains("ok"),
        "doctor checks should pass: {}",
        output
    );
}

#[test]
fn test_push_without_init_fails() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_failure(&["push", "main"]);
    insta::assert_snapshot!(output, @"jjq: jjq is not initialized. Run 'jjq init' first.");
}

#[test]
fn test_config_without_init_fails() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_failure(&["config"]);
    insta::assert_snapshot!(output, @"jjq: jjq is not initialized. Run 'jjq init' first.");
}

#[test]
fn test_tail_no_log_file() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let output = repo.jjq_output(&["tail", "--no-follow"]);
    assert!(
        output.contains("no run output available"),
        "should report no log file: {}",
        output
    );
}

#[test]
fn test_tail_after_run() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_with_check("echo hello-from-check");

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("newfile.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "simple-branch"]);
    repo.jjq_success(&["push", "simple-branch"]);
    repo.jjq_success(&["run"]);

    let output = repo.jjq_output(&["tail", "--no-follow"]);
    assert!(
        output.contains("hello-from-check"),
        "tail should show check output: {}",
        output
    );
}

#[test]
fn test_tail_all_flag() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_with_check("echo line1 && echo line2 && echo line3");

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("newfile.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "simple-branch"]);
    repo.jjq_success(&["push", "simple-branch"]);
    repo.jjq_success(&["run"]);

    let output = repo.jjq_output(&["tail", "--all", "--no-follow"]);
    assert!(output.contains("line1"), "should show all output: {}", output);
    assert!(output.contains("line2"), "should show all output: {}", output);
    assert!(output.contains("line3"), "should show all output: {}", output);
}

#[test]
fn test_run_failure_shows_output() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge_with_check("echo 'build failed: missing dependency' && exit 1");

    run_jj(repo.path(), &["new", "-m", "will fail", "main"]);
    fs::write(repo.path().join("file.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fail-branch"]);
    repo.jjq_success(&["push", "fail-branch"]);

    let output = repo.jjq_failure(&["run"]);
    assert!(
        output.contains("build failed: missing dependency"),
        "should show check command output on failure: {}",
        output
    );
}

#[test]
fn test_run_creates_log_file() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge();

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("newfile.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "simple-branch"]);
    repo.jjq_success(&["push", "simple-branch"]);
    repo.jjq_success(&["run"]);

    let log_path = repo.path().join(".jj").join("jjq-run.log");
    assert!(log_path.exists(), "run log file should exist after run");

    let contents = fs::read_to_string(&log_path).unwrap();
    assert!(
        contents.contains("--- jjq: run complete"),
        "log should contain sentinel: {}",
        contents
    );
}

#[test]
fn test_log_file_truncated_between_runs() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge_with_check("echo run-output-marker");

    let log_path = repo.path().join(".jj").join("jjq-run.log");
    // Seed a stale log to verify run truncates rather than appends.
    fs::write(
        &log_path,
        "stale-marker\n--- jjq: run complete\nstale-tail\n--- jjq: run complete\n",
    )
    .unwrap();

    run_jj(repo.path(), &["new", "-m", "feature 1", "main"]);
    fs::write(repo.path().join("f1.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);
    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["run"]);

    let contents = fs::read_to_string(&log_path).unwrap();

    assert!(
        !contents.contains("stale-marker"),
        "stale contents should be truncated away: {}",
        contents
    );

    // Log should contain exactly one sentinel (file was truncated)
    let sentinel_count = contents.matches("--- jjq: run complete").count();
    assert_eq!(
        sentinel_count, 1,
        "log should contain exactly one sentinel after truncation, got {}: {}",
        sentinel_count, contents
    );
}

#[test]
fn test_run_rebase_success() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq(); // defaults to rebase strategy

    // Create a simple non-conflicting change
    run_jj(repo.path(), &["new", "-m", "add feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);
    run_jj(repo.path(), &["new", "main"]);

    // Push and run
    repo.jjq_success(&["push", "feature"]);
    let output = repo.jjq_success(&["run"]);

    // Verify rebase strategy was used
    assert!(output.contains("rebase strategy"), "expected rebase strategy in output: {}", output);
    assert!(output.contains("rebased"), "expected 'rebased' in output: {}", output);

    // Verify trunk advanced: feature.txt should exist on main
    assert!(repo.jj_file_exists("feature.txt", "main"));
}

#[test]
fn test_run_rebase_preserves_change_id() {
    // Option E: verify the original change ID is preserved when landing
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a change and capture its change ID
    run_jj(repo.path(), &["new", "-m", "my feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);

    let original_change_id = run_jj(repo.path(), &[
        "log", "-r", "feat", "--no-graph", "-T", "change_id.short()",
    ]).trim().to_string();

    run_jj(repo.path(), &["new", "main"]);

    // Push and run
    repo.jjq_success(&["push", "feat"]);
    repo.jjq_success(&["run"]);

    // The change ID on main should be the SAME as the original
    let landed_change_id = run_jj(repo.path(), &[
        "log", "-r", "main", "--no-graph", "-T", "change_id.short()",
    ]).trim().to_string();

    assert_eq!(
        original_change_id, landed_change_id,
        "change ID should be preserved: original={}, landed={}",
        original_change_id, landed_change_id
    );
}

#[test]
fn test_run_rebase_linear_history() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Push two non-conflicting changes
    run_jj(repo.path(), &["new", "-m", "feature A", "main"]);
    fs::write(repo.path().join("a.txt"), "aaa\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat-a"]);

    run_jj(repo.path(), &["new", "-m", "feature B", "main"]);
    fs::write(repo.path().join("b.txt"), "bbb\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat-b"]);

    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat-a"]);
    repo.jjq_success(&["push", "feat-b"]);
    repo.jjq_success(&["run", "--all"]);

    // Verify both files landed
    assert!(repo.jj_file_exists("a.txt", "main"));
    assert!(repo.jj_file_exists("b.txt", "main"));

    // Verify linear history: main's commit should have only 1 parent
    let parent_count = run_jj(repo.path(), &[
        "log", "-r", "main", "--no-graph", "-T", "parents.len()",
    ]);
    assert_eq!(parent_count.trim(), "1", "expected linear history (1 parent), got: {}", parent_count.trim());
}

#[test]
fn test_run_rebase_preserves_description() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "my cool feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);
    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat"]);
    repo.jjq_success(&["run"]);

    // Check that trunk's description contains the original message + trailers
    // Note: Option E preserves the original change ID, so no jjq-source trailer needed
    let desc = run_jj(repo.path(), &[
        "log", "-r", "main", "--no-graph", "-T", "description",
    ]);
    assert!(desc.contains("my cool feature"), "desc should contain original: {}", desc);
    assert!(desc.contains("jjq-sequence:"), "desc should have sequence trailer: {}", desc);
    assert!(desc.contains("jjq-strategy: rebase"), "desc should have strategy trailer: {}", desc);
}

#[test]
fn test_run_rebase_conflict() {
    let repo = TestRepo::with_prs();
    repo.init_jjq(); // rebase strategy

    // PR1 and PR2 both modify main.go, should conflict after PR1 merges
    repo.jjq_success(&["push", "pr1"]);
    repo.jjq_success(&["push", "pr2"]);

    // Run first item (should succeed)
    repo.jjq_success(&["run"]);

    // Run second item (should fail with conflict)
    let output = repo.jjq_failure(&["run"]);
    assert!(output.contains("conflicts"), "expected conflict: {}", output);
}

#[test]
fn test_run_rebase_check_failure() {
    let repo = TestRepo::with_go_project();
    // Use rebase strategy with "false" check command
    repo.jjq_success(&["init", "--trunk", "main", "--check", "false"]);

    run_jj(repo.path(), &["new", "-m", "will fail check", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);
    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat"]);
    let output = repo.jjq_failure(&["run"]);
    assert!(output.contains("failed check"), "expected check failure: {}", output);

    // Trunk should NOT have moved
    assert!(!repo.jj_file_exists("feature.txt", "main"));
}

#[test]
fn test_run_rebase_commit_chain() {
    // Regression test: pushing an empty @ whose parent has the actual changes.
    // duplicate_onto must duplicate the full range (trunk..candidate), not just
    // the candidate's diff, to include intermediate commits.
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a change, then do "jj new" so @ is empty and the parent has content.
    // This mimics: workspace with work in @-, empty @ on top.
    run_jj(repo.path(), &["new", "-m", "add feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    // Create a new empty commit on top (simulates jj's empty working copy)
    run_jj(repo.path(), &["new"]);
    // Now @ is empty, @- has the content. Push @ (the empty tip).
    repo.jjq_success(&["push", "@"]);
    let output = repo.jjq_success(&["run"]);

    assert!(output.contains("rebased"), "expected rebased: {}", output);
    // The critical check: feature.txt must be on main even though the pushed
    // commit itself was empty — the intermediate commit's changes must be included.
    assert!(repo.jj_file_exists("feature.txt", "main"),
        "feature.txt should be on main after rebase of commit chain");
}

#[test]
fn test_run_rebase_abandons_all_duplicates() {
    // Regression test: when duplicating a commit chain for rebase testing,
    // ALL duplicates must be abandoned on success, not just the tip.
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a chain of 3 commits off main
    run_jj(repo.path(), &["new", "-m", "first", "main"]);
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    run_jj(repo.path(), &["new", "-m", "second"]);
    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    run_jj(repo.path(), &["new", "-m", "third"]);
    fs::write(repo.path().join("c.txt"), "c\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);
    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat"]);
    repo.jjq_success(&["run"]);

    // Count commits with each description — should be exactly 1 of each
    let log = run_jj(repo.path(), &["log", "--no-graph", "-T", "description.first_line() ++ \"\\n\""]);
    let first_count = log.lines().filter(|l| *l == "first").count();
    let second_count = log.lines().filter(|l| *l == "second").count();
    let third_count = log.lines().filter(|l| *l == "third").count();

    assert_eq!(first_count, 1, "should have exactly 1 'first' commit, found {}", first_count);
    assert_eq!(second_count, 1, "should have exactly 1 'second' commit, found {}", second_count);
    assert_eq!(third_count, 1, "should have exactly 1 'third' commit, found {}", third_count);

    // Verify all files are on main
    assert!(repo.jj_file_exists("a.txt", "main"), "a.txt should be on main");
    assert!(repo.jj_file_exists("b.txt", "main"), "b.txt should be on main");
    assert!(repo.jj_file_exists("c.txt", "main"), "c.txt should be on main");
}

#[test]
fn test_run_skips_empty_commit_rebase() {
    // When a queued commit's changes are already on trunk, run should skip it.
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a feature branch
    run_jj(repo.path(), &["new", "-m", "add feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);

    // Manually apply the same change to trunk (simulates someone else landing it)
    run_jj(repo.path(), &["new", "-m", "same change on trunk", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "set", "main"]);

    // Go back to a clean working copy
    run_jj(repo.path(), &["new", "main"]);

    // Push the feature — its changes are already on trunk
    repo.jjq_success(&["push", "feat"]);
    let output = repo.jjq_success(&["run"]);

    assert!(output.contains("empty"), "expected 'empty' in output: {}", output);
    assert!(output.contains("skipping"), "expected 'skipping' in output: {}", output);

    // Queue should now be empty
    let status = repo.jjq_success(&["status"]);
    assert!(status.contains("queue is empty"), "queue should be empty: {}", status);
}

#[test]
fn test_run_skips_empty_commit_merge() {
    // Same scenario with merge strategy.
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge();

    // Create a feature branch
    run_jj(repo.path(), &["new", "-m", "add feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);

    // Manually apply the same change to trunk
    run_jj(repo.path(), &["new", "-m", "same change on trunk", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "set", "main"]);

    // Go back to a clean working copy
    run_jj(repo.path(), &["new", "main"]);

    // Push the feature — its changes are already on trunk
    repo.jjq_success(&["push", "feat"]);
    let output = repo.jjq_success(&["run"]);

    assert!(output.contains("empty"), "expected 'empty' in output: {}", output);
    assert!(output.contains("skipping"), "expected 'skipping' in output: {}", output);
}

#[test]
fn test_run_all_counts_skipped_empty() {
    // Batch mode should report skipped items separately.
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create first feature (will be empty — changes already on trunk)
    run_jj(repo.path(), &["new", "-m", "feature one", "main"]);
    fs::write(repo.path().join("f1.txt"), "one\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat1"]);

    // Apply same change to trunk
    run_jj(repo.path(), &["new", "-m", "same as f1", "main"]);
    fs::write(repo.path().join("f1.txt"), "one\n").unwrap();
    run_jj(repo.path(), &["bookmark", "set", "main"]);

    // Create second feature (has real changes)
    run_jj(repo.path(), &["new", "-m", "feature two", "main"]);
    fs::write(repo.path().join("f2.txt"), "two\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat2"]);

    // Go back to clean working copy
    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat1"]);
    repo.jjq_success(&["push", "feat2"]);
    let output = repo.jjq_success(&["run", "--all"]);

    assert!(output.contains("skipped (empty)"), "expected skipped count in output: {}", output);
    assert!(repo.jj_file_exists("f2.txt", "main"), "f2.txt should be on main");
}

// ============================================================================
// Requeue tests

#[test]
fn test_requeue_basic() {
    // Set up: push a feature with a failing check, then requeue after
    // switching to a passing check command.
    let repo = TestRepo::with_go_project();
    repo.init_jjq_with_check("false");

    // Create a non-conflicting feature
    run_jj(repo.path(), &["new", "-m", "add feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);
    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat"]);

    // Run — fails because check command is "false"
    repo.jjq_output(&["run"]);

    // Verify item 1 is in failed state
    let status = repo.jjq_success(&["status"]);
    assert!(status.contains("Failed"), "expected failed item: {}", status);

    // Switch check command to "true" (simulating the user fixing the issue)
    repo.jjq_success(&["config", "check_command", "true"]);

    // Requeue the failed item
    let output = repo.jjq_success(&["requeue", "1"]);
    insta::assert_snapshot!(output, @"jjq: requeued failed item 1 as 2 (trunk: main in <REPO>)");

    // Status should show the item back in queue, no failed items
    let status = repo.jjq_success(&["status"]);
    assert!(status.contains("Queued"), "expected queued item: {}", status);
    assert!(!status.contains("Failed"), "failed list should be empty: {}", status);

    // Run the requeued item — should succeed now
    let run_output = repo.jjq_success(&["run"]);
    assert!(run_output.contains("merged") || run_output.contains("rebased"),
        "expected successful merge: {}", run_output);
}

#[test]
fn test_requeue_not_found() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let output = repo.jjq_failure(&["requeue", "999"]);
    insta::assert_snapshot!(output, @"jjq: failed item 999 not found");
}

#[test]
fn test_requeue_already_queued() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create and push a feature
    run_jj(repo.path(), &["new", "-m", "feature", "main"]);
    fs::write(repo.path().join("new.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);
    run_jj(repo.path(), &["new", "main"]);

    repo.jjq_success(&["push", "feat"]);

    // Try requeuing ID 1 which is queued, not failed
    let output = repo.jjq_failure(&["requeue", "1"]);
    insta::assert_snapshot!(output, @"jjq: item 1 is already queued");
}

#[test]
fn test_requeue_still_conflicts() {
    // Set up a failed item but don't fix the conflict before requeuing
    let repo = TestRepo::with_prs();
    repo.init_jjq_merge_with_check("true");

    repo.jjq_success(&["push", "pr1"]);
    repo.jjq_success(&["push", "pr2"]);

    // Run pr1 (succeeds), then pr2 (fails with conflict)
    repo.jjq_success(&["run"]);
    repo.jjq_output(&["run"]);

    // Try requeuing without resolving conflicts — should fail
    let output = repo.jjq_failure(&["requeue", "2"]);
    assert!(output.contains("conflicts"), "expected conflict error: {}", output);
}

#[test]
fn test_status_shows_recently_landed_merge() {
    let repo = TestRepo::with_run_all_happy_scenario();
    repo.init_jjq_merge();

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // Run all items
    repo.jjq_success(&["run", "--all"]);

    // Status should show recently landed items (most recent first)
    let status = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status, @r"
    jjq: Landed (recent):
      3: <CHANGE_ID> Success: merge 3
      2: <CHANGE_ID> Success: merge 2
      1: <CHANGE_ID> Success: merge 1
    ");
}

#[test]
fn test_status_shows_recently_landed_rebase() {
    let repo = TestRepo::with_run_all_happy_scenario();
    repo.init_jjq();

    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["push", "f3"]);

    // Run all items
    repo.jjq_success(&["run", "--all"]);

    // Status should show recently landed items (most recent first, rebase shows original descriptions)
    let status = repo.jjq_success(&["status"]);
    insta::assert_snapshot!(status, @r"
    jjq: Landed (recent):
      3: <CHANGE_ID> feature 3
      2: <CHANGE_ID> feature 2
      1: <CHANGE_ID> feature 1
    ");
}

/// Add a secondary workspace with the given name, returning its root path.
/// The workspace directory is created inside `container` so it outlives the call.
fn add_workspace(repo: &TestRepo, container: &TempDir, name: &str) -> PathBuf {
    let ws_path = container.path().join(name);
    run_jj(
        repo.path(),
        &["workspace", "add", "--name", name, ws_path.to_str().unwrap()],
    );
    ws_path
}

#[test]
fn test_push_from_secondary_workspace_uses_shared_locks() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a branch to push
    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);

    let container = TempDir::new().expect("failed to create workspace container");
    let ws_path = add_workspace(&repo, &container, "w2");

    let output = repo
        .jjq_in(&ws_path)
        .args(["push", "feature"])
        .output()
        .expect("failed to run jjq");
    assert!(
        output.status.success(),
        "push from secondary workspace failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The queue entry was created
    let bookmarks = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/queue/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    assert!(
        bookmarks.contains("jjq/queue/000001"),
        "queue entry should exist after push from secondary workspace: {}",
        bookmarks
    );

    // Locks must live in the primary workspace's .jj so all workspaces share them
    let primary_locks = repo.path().join(".jj").join("jjq-locks");
    assert!(
        primary_locks.join("meta.lock").exists(),
        "meta lock should be in the primary workspace's lock dir: {}",
        primary_locks.display()
    );
    let secondary_locks = ws_path.join(".jj").join("jjq-locks");
    assert!(
        !secondary_locks.exists(),
        "secondary workspace must not get a private lock dir: {}",
        secondary_locks.display()
    );
}

#[test]
fn test_push_from_secondary_workspace_respects_meta_lock() {
    use fs2::FileExt;

    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Create a branch to push
    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);

    let container = TempDir::new().expect("failed to create workspace container");
    let ws_path = add_workspace(&repo, &container, "w2");

    // Hold the shared metadata lock, as a concurrent writer would
    let lock_dir = repo.path().join(".jj").join("jjq-locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock_file = fs::File::create(lock_dir.join("meta.lock")).unwrap();
    lock_file.lock_exclusive().unwrap();

    // A push from the secondary workspace must observe the held lock
    let output = repo
        .jjq_in(&ws_path)
        .env("JJQ_META_LOCK_TIMEOUT_MS", "500")
        .args(["push", "feature"])
        .output()
        .expect("failed to run jjq");
    assert_eq!(
        output.status.code(),
        Some(3),
        "push should exit LOCK_HELD while the meta lock is held; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // LOCK_HELD is raised before bookmark creation, so the queue must be empty
    let bookmarks = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/queue/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    assert!(
        bookmarks.trim().is_empty(),
        "no queue entry should be created while the meta lock is held: {}",
        bookmarks
    );
}

/// Add a secondary workspace based at `rev`, returning its root path.
/// The workspace directory is created inside `container` so it outlives the call.
fn add_workspace_at(repo: &TestRepo, container: &TempDir, name: &str, rev: &str) -> PathBuf {
    let ws_path = container.path().join(name);
    run_jj(
        repo.path(),
        &[
            "workspace",
            "add",
            "--name",
            name,
            "-r",
            rev,
            ws_path.to_str().unwrap(),
        ],
    );
    ws_path
}

/// Run jjq in `dir` expecting success, returning combined stdout+stderr.
fn jjq_success_in(repo: &TestRepo, dir: &Path, args: &[&str]) -> String {
    let output = repo
        .jjq_in(dir)
        .args(args)
        .output()
        .expect("failed to run jjq");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "jjq {:?} in {} failed: {}{}",
        args,
        dir.display(),
        stdout,
        stderr
    );
    format!("{}{}", stdout, stderr)
}

/// Commit ID a revset resolves to, via the primary workspace.
fn commit_id_of(repo: &TestRepo, revset: &str) -> String {
    run_jj(
        repo.path(),
        &["log", "-r", revset, "--no-graph", "-T", "commit_id"],
    )
    .trim()
    .to_string()
}

/// Regression: landing a commit that is a live workspace's working-copy
/// commit leaves the trunk head as that working copy; the workspace's next
/// snapshot amends the trunk head and the trunk bookmark follows the rewrite,
/// silently absorbing unchecked edits. push moves the working copy to a new
/// change so post-landing edits stay off the landed commit.
#[test]
fn test_rebase_landing_wc_commit_post_edit_does_not_move_trunk() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq(); // rebase strategy

    let container = TempDir::new().expect("failed to create workspace container");
    let ws = add_workspace_at(&repo, &container, "agent2", "main");

    fs::write(ws.join("feature.txt"), "checked\n").unwrap();
    run_jj(&ws, &["describe", "-m", "agent2 feature"]);

    jjq_success_in(&repo, &ws, &["push", "@"]);
    jjq_success_in(&repo, &ws, &["run"]);

    let main_after_land = commit_id_of(&repo, "main");
    assert!(
        repo.jj_file_exists("feature.txt", "main"),
        "feature.txt should have landed on main"
    );

    // Post-landing edit in the workspace must not move main.
    fs::write(ws.join("feature.txt"), "checked\nUNCHECKED\n").unwrap();
    run_jj(&ws, &["status"]); // forces a snapshot

    let main_after_edit = commit_id_of(&repo, "main");
    assert_eq!(
        main_after_land, main_after_edit,
        "post-landing edit in the pushing workspace moved main"
    );
    let landed = run_jj(repo.path(), &["file", "show", "feature.txt", "-r", "main"]);
    assert_eq!(
        landed, "checked\n",
        "main absorbed an unchecked post-landing edit"
    );
}

/// Same hazard under the merge strategy: amending the landed candidate
/// auto-rebases the merge commit above it and the trunk bookmark follows.
#[test]
fn test_merge_landing_wc_commit_post_edit_does_not_move_trunk() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_merge();

    let container = TempDir::new().expect("failed to create workspace container");
    let ws = add_workspace_at(&repo, &container, "agent2", "main");

    fs::write(ws.join("feature.txt"), "checked\n").unwrap();
    run_jj(&ws, &["describe", "-m", "agent2 feature"]);

    jjq_success_in(&repo, &ws, &["push", "@"]);
    jjq_success_in(&repo, &ws, &["run"]);

    let main_after_land = commit_id_of(&repo, "main");

    fs::write(ws.join("feature.txt"), "checked\nUNCHECKED\n").unwrap();
    run_jj(&ws, &["status"]); // forces a snapshot

    let main_after_edit = commit_id_of(&repo, "main");
    assert_eq!(
        main_after_land, main_after_edit,
        "post-landing edit in the pushing workspace moved main"
    );
    let landed = run_jj(repo.path(), &["file", "show", "feature.txt", "-r", "main"]);
    assert_eq!(
        landed, "checked\n",
        "main absorbed an unchecked post-landing edit"
    );
}

/// Pushing the working-copy commit moves the working copy to a new empty
/// child (like `jj commit` does), leaving the queued commit as pushed.
#[test]
fn test_push_moves_working_copy_off_queued_commit() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    let candidate_commit = commit_id_of(&repo, "@");

    let output = repo.jjq_success(&["push", "@"]);
    assert!(
        output.contains("working copy moved to a new change"),
        "push of @ should report the working-copy move: {}",
        output
    );

    let wc_commit = commit_id_of(&repo, "@");
    assert_ne!(
        wc_commit, candidate_commit,
        "working copy should have moved off the queued commit"
    );
    let wc_parent = run_jj(
        repo.path(),
        &["log", "-r", "@", "--no-graph", "-T", "parents.map(|c| c.commit_id()).join(\"\")"],
    )
    .trim()
    .to_string();
    assert_eq!(
        wc_parent, candidate_commit,
        "working copy should be a child of the queued commit"
    );
    let queued_commit = commit_id_of(&repo, "bookmarks(exact:\"jjq/queue/000001\")");
    assert_eq!(
        queued_commit, candidate_commit,
        "queue entry should point at the original commit"
    );
}

/// Pushing a revset that is not the working-copy commit leaves the working
/// copy alone.
#[test]
fn test_push_leaves_working_copy_when_not_wc_commit() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "hello\n").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feat"]);
    run_jj(repo.path(), &["new", "main"]);
    let wc_before = commit_id_of(&repo, "@");

    let output = repo.jjq_success(&["push", "feat"]);
    assert!(
        !output.contains("working copy moved"),
        "push of a non-@ revset should not move the working copy: {}",
        output
    );
    let wc_after = commit_id_of(&repo, "@");
    assert_eq!(wc_before, wc_after, "working copy should not have moved");
}

/// requeue of a fixed candidate that is the working-copy commit moves the
/// working copy off it, same as push.
#[test]
fn test_requeue_moves_working_copy_off_requeued_commit() {
    let repo = TestRepo::with_go_project();
    repo.jjq_success(&["init", "--trunk", "main", "--check", "false"]);

    run_jj(repo.path(), &["new", "-m", "fixme", "main"]);
    fs::write(repo.path().join("feature.txt"), "broken\n").unwrap();
    let candidate_change = run_jj(
        repo.path(),
        &["log", "-r", "@", "--no-graph", "-T", "change_id.short()"],
    )
    .trim()
    .to_string();

    repo.jjq_success(&["push", "@"]);
    repo.jjq_failure(&["run"]); // check command "false" fails the item

    // Fix the candidate in place, then requeue it.
    run_jj(repo.path(), &["edit", &candidate_change]);
    fs::write(repo.path().join("feature.txt"), "fixed\n").unwrap();
    repo.jjq_success(&["config", "check_command", "true"]);

    let output = repo.jjq_success(&["requeue", "1"]);
    assert!(
        output.contains("working copy moved to a new change"),
        "requeue of @ should report the working-copy move: {}",
        output
    );

    let candidate_commit = commit_id_of(&repo, &candidate_change);
    let wc_commit = commit_id_of(&repo, "@");
    assert_ne!(
        wc_commit, candidate_commit,
        "working copy should have moved off the requeued commit"
    );
}

/// When the landed commit is another workspace's live working-copy commit
/// (pushed from elsewhere, so push could not move that working copy), the
/// runner warns that edits in that workspace will drag trunk.
#[test]
fn test_run_warns_when_landing_another_workspaces_wc_commit() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let container = TempDir::new().expect("failed to create workspace container");
    let ws = add_workspace_at(&repo, &container, "agent2", "main");

    fs::write(ws.join("feature.txt"), "checked\n").unwrap();
    run_jj(&ws, &["describe", "-m", "agent2 feature"]);

    // Push agent2's working-copy commit from the primary workspace: push
    // cannot move agent2's working copy, so the runner must warn instead.
    let push_output = repo.jjq_success(&["push", "agent2@"]);
    assert!(
        !push_output.contains("working copy moved"),
        "push from another workspace should not move any working copy: {}",
        push_output
    );

    let run_output = repo.jjq_success(&["run"]);
    assert!(
        run_output.contains("workspace 'agent2'"),
        "run should warn about landing agent2's working-copy commit: {}",
        run_output
    );
    assert!(
        run_output.contains("jj new"),
        "warning should advise running jj new in the workspace: {}",
        run_output
    );
}

/// Spawn a `jjq push` child in `dir` with a generous metadata lock timeout,
/// so a loaded test machine cannot turn ordinary lock waiting into a
/// spurious exit 3. Callers wait via wait_with_output.
fn spawn_push(repo: &TestRepo, dir: &Path, revset: &str) -> process::Child {
    let mut cmd = repo.jjq_std();
    cmd.current_dir(dir)
        .env("JJQ_META_LOCK_TIMEOUT_MS", "60000")
        .args(["push", revset])
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    cmd.spawn().expect("failed to spawn jjq push")
}

/// Queue bookmark names in the repo, sorted. `jj bookmark list -r` returns
/// every bookmark on the matched commits (including the user's own), hence
/// the name filter.
fn queue_bookmark_names(repo: &TestRepo) -> Vec<String> {
    let out = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/queue/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    let mut names: Vec<String> = out
        .lines()
        .filter(|l| l.starts_with("jjq/queue/"))
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// Assert no bookmark in the repo is conflicted (racing metadata writers
/// used to conflict jjq/_/_).
fn assert_no_conflicted_bookmarks(repo: &TestRepo) {
    let all = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-T",
            "name ++ if(conflict, \" CONFLICTED\") ++ \"\\n\"",
        ],
    );
    assert!(
        !all.to_lowercase().contains("conflicted"),
        "no bookmark should be conflicted: {}",
        all
    );
}

/// The sequence counter on the metadata branch.
fn meta_last_id(repo: &TestRepo) -> String {
    run_jj(
        repo.path(),
        &["file", "show", "last_id", "-r", "bookmarks(exact:\"jjq/_/_\")"],
    )
    .trim()
    .to_string()
}

/// THE regression test for the original metadata corruption: two pushes
/// racing from two workspaces used to allocate the same sequence ID and
/// conflict the metadata bookmark. The blocking meta lock serializes them —
/// the loser waits instead of failing — so simply spawning both and waiting
/// is deterministic; no sleeps needed.
#[test]
fn test_concurrent_pushes_from_two_workspaces() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Two distinct changes to push.
    run_jj(repo.path(), &["new", "-m", "feature a", "main"]);
    fs::write(repo.path().join("a.txt"), "content a").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fa"]);
    run_jj(repo.path(), &["new", "-m", "feature b", "main"]);
    fs::write(repo.path().join("b.txt"), "content b").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "fb"]);
    run_jj(repo.path(), &["new", "main"]);

    let container = TempDir::new().expect("failed to create workspace container");
    let ws_a = add_workspace(&repo, &container, "wa");
    let ws_b = add_workspace(&repo, &container, "wb");

    let child_a = spawn_push(&repo, &ws_a, "fa");
    let child_b = spawn_push(&repo, &ws_b, "fb");

    let out_a = child_a.wait_with_output().expect("failed to wait for push a");
    let out_b = child_b.wait_with_output().expect("failed to wait for push b");
    for (name, out) in [("fa", &out_a), ("fb", &out_b)] {
        assert!(
            out.status.success(),
            "push {} should succeed (exit {:?}); stdout: {} stderr: {}",
            name,
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    assert_eq!(
        queue_bookmark_names(&repo),
        vec!["jjq/queue/000001", "jjq/queue/000002"],
        "racing pushes must allocate unique consecutive sequence IDs"
    );
    assert_no_conflicted_bookmarks(&repo);
    assert_eq!(
        meta_last_id(&repo),
        "2",
        "sequence counter should equal the number of pushes"
    );
}

/// Push storm: six distinct changes pushed from three workspaces
/// concurrently — three parallel streams of two sequential pushes each.
/// Pushes within one workspace are sequential because jj itself does not
/// support concurrent commands mutating a single working copy (racing
/// snapshots diverge the workspace's operation pointer, failing with a
/// "sibling operation" error independent of jjq's locking). Ignored by
/// default because it spawns many concurrent jj processes; run it manually
/// with:
///   cargo test -- --ignored stress_concurrent_push_storm
#[test]
#[ignore = "manual stress test; run with: cargo test -- --ignored stress_concurrent_push_storm"]
fn stress_concurrent_push_storm() {
    const WORKSPACES: usize = 3;
    const PUSHES_PER_WORKSPACE: usize = 2;
    const N: usize = WORKSPACES * PUSHES_PER_WORKSPACE;

    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    for i in 0..N {
        run_jj(
            repo.path(),
            &["new", "-m", &format!("storm feature {}", i), "main"],
        );
        fs::write(repo.path().join(format!("storm-{}.txt", i)), "content").unwrap();
        run_jj(repo.path(), &["bookmark", "create", &format!("storm-{}", i)]);
    }
    run_jj(repo.path(), &["new", "main"]);

    let container = TempDir::new().expect("failed to create workspace container");
    let workspaces: Vec<PathBuf> = (0..WORKSPACES)
        .map(|i| add_workspace(&repo, &container, &format!("storm-ws-{}", i)))
        .collect();

    std::thread::scope(|scope| {
        for (w, ws_path) in workspaces.iter().enumerate() {
            let repo = &repo;
            scope.spawn(move || {
                for k in 0..PUSHES_PER_WORKSPACE {
                    let i = w + k * WORKSPACES;
                    let child = spawn_push(repo, ws_path, &format!("storm-{}", i));
                    let out = child.wait_with_output().expect("failed to wait for push");
                    assert!(
                        out.status.success(),
                        "storm push {} should succeed (exit {:?}); stdout: {} stderr: {}",
                        i,
                        out.status.code(),
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr),
                    );
                }
            });
        }
    });

    // Contiguous unique ID set 1..=N, all still queued.
    let expected: Vec<String> = (1..=N).map(|id| format!("jjq/queue/{:06}", id)).collect();
    assert_eq!(
        queue_bookmark_names(&repo),
        expected,
        "storm pushes must allocate a contiguous unique sequence ID set"
    );
    assert_no_conflicted_bookmarks(&repo);
    assert_eq!(
        meta_last_id(&repo),
        N.to_string(),
        "sequence counter should equal the number of pushes"
    );
}

/// Set up a repo with one queued item and the run lock held by this process,
/// as if another queue runner were active. The returned File holds the lock.
fn repo_with_queued_item_and_held_run_lock() -> (TestRepo, fs::File) {
    use fs2::FileExt;

    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);
    repo.jjq_success(&["push", "feature"]);

    let lock_dir = repo.path().join(".jj").join("jjq-locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock_file = fs::File::create(lock_dir.join("run.lock")).unwrap();
    lock_file.lock_exclusive().unwrap();

    (repo, lock_file)
}

/// Assert the first queued item is still present and untouched.
fn assert_queue_item_remains(repo: &TestRepo) {
    let bookmarks = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/queue/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    assert!(
        bookmarks.contains("jjq/queue/000001"),
        "queued item should remain untouched: {}",
        bookmarks
    );
}

/// Assert a lock-held run exited CONFLICT with a single short notice.
fn assert_run_lock_notice(combined: &str, code: Option<i32>) {
    assert_eq!(
        code,
        Some(1),
        "should exit CONFLICT while the run lock is held (None means killed at timeout); output: {}",
        combined
    );
    assert_eq!(
        combined.matches("another queue runner is active").count(),
        1,
        "notice should appear exactly once: {}",
        combined
    );
    assert!(
        combined.lines().count() < 5,
        "output should be a short notice, not a retry log: {}",
        combined
    );
}

#[test]
fn test_run_all_exits_when_run_lock_held() {
    let (repo, _lock_file) = repo_with_queued_item_and_held_run_lock();

    // A second runner must notice the held lock and exit at once — not spin
    // on the lock or count the contention as item failures. The timeout
    // bounds the test if a regression reintroduces the retry loop.
    let (stdout, stderr, code) =
        repo.jjq_with_timeout(&["run", "--all"], std::time::Duration::from_secs(10));
    let combined = format!("{}{}", stdout, stderr);

    assert_run_lock_notice(&combined, code);
    assert_queue_item_remains(&repo);
}

#[test]
fn test_run_exits_when_run_lock_held() {
    let (repo, _lock_file) = repo_with_queued_item_and_held_run_lock();

    // A single run must also observe the lock before touching the queue
    let (stdout, stderr, code) =
        repo.jjq_with_timeout(&["run"], std::time::Duration::from_secs(10));
    let combined = format!("{}{}", stdout, stderr);

    assert_run_lock_notice(&combined, code);
    assert_queue_item_remains(&repo);
}

#[test]
fn test_run_all_missing_check_command_fails_fast() {
    let repo = TestRepo::with_go_project();
    // Initialize metadata without a check command: setting any config key
    // auto-creates the metadata branch, and only init enforces --check.
    repo.jjq_success(&["config", "trunk_bookmark", "main"]);

    run_jj(repo.path(), &["new", "-m", "feature", "main"]);
    fs::write(repo.path().join("f1.txt"), "feature").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);
    repo.jjq_success(&["push", "f1"]);

    // Without a check command the run must fail once, promptly — a
    // regression here loops forever re-reading the same queue item.
    let (stdout, stderr, code) =
        repo.jjq_with_timeout(&["run", "--all"], std::time::Duration::from_secs(10));
    let combined = format!("{}{}", stdout, stderr);

    assert_eq!(
        code,
        Some(1),
        "run --all without check_command should exit CONFLICT (None means killed at timeout): {}",
        combined
    );
    assert_eq!(
        combined.matches("check_command not configured").count(),
        1,
        "notice should appear exactly once: {}",
        combined
    );
    assert_queue_item_remains(&repo);
}

#[test]
fn test_push_times_out_when_meta_lock_held() {
    use fs2::FileExt;

    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);

    // Hold the metadata writer lock, as a stuck concurrent writer would
    let lock_dir = repo.path().join(".jj").join("jjq-locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock_file = fs::File::create(lock_dir.join("meta.lock")).unwrap();
    lock_file.lock_exclusive().unwrap();

    // Push waits for the lock (timeout shortened via env) and then gives up
    // with LOCK_HELD rather than racing the writer on the metadata bookmark
    let output = repo
        .jjq()
        .env("JJQ_META_LOCK_TIMEOUT_MS", "500")
        .args(["push", "feature"])
        .output()
        .expect("failed to run jjq");
    assert_eq!(
        output.status.code(),
        Some(3),
        "push should exit LOCK_HELD while the meta lock stays held; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not acquire metadata lock"),
        "should explain the metadata lock timeout: {}",
        stderr
    );

    // LOCK_HELD is raised before bookmark creation, so the queue must be empty
    let bookmarks = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/queue/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    assert!(
        bookmarks.trim().is_empty(),
        "no queue entry should be created while the meta lock is held: {}",
        bookmarks
    );
}

/// Force the jjq metadata bookmark into a conflicted state whose sides are
/// the two given commits, by moving it to each in divergent operations (fork
/// the op log with --at-operation), as racing writers without shared locks
/// would. Neither side may equal the bookmark's current target, or jj
/// resolves the conflict trivially.
fn conflict_meta_bookmark(repo: &TestRepo, side_a: &str, side_b: &str) {
    let op = run_jj(
        repo.path(),
        &["op", "log", "-n1", "--no-graph", "-T", "id.short() ++ \"\\n\""],
    );
    run_jj(
        repo.path(),
        &["bookmark", "set", "jjq/_/_", "-r", side_a, "--allow-backwards"],
    );
    run_jj(
        repo.path(),
        &[
            "--at-operation",
            op.trim(),
            "bookmark",
            "set",
            "jjq/_/_",
            "-r",
            side_b,
            "--allow-backwards",
        ],
    );
    // Any jj command merges the divergent operations, conflicting the bookmark.
    run_jj(repo.path(), &["status"]);
}

#[test]
fn test_conflicted_metadata_bookmark_reports_cleanly() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    let side_a = run_jj(repo.path(), &["log", "-r", "main", "--no-graph", "-T", "commit_id"]);
    run_jj(repo.path(), &["new", "-m", "divergent side", "main"]);
    let side_b = run_jj(repo.path(), &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    conflict_meta_bookmark(&repo, side_a.trim(), side_b.trim());

    // Reads must fail with a short, actionable message — not a wall of raw
    // jj template errors.
    let config_out = repo.jjq_failure(&["config", "trunk_bookmark"]);
    assert!(
        config_out.contains("run 'jjq doctor'"),
        "config read should point at doctor: {}",
        config_out
    );
    assert!(
        !config_out.contains("short-prefixes"),
        "no raw jj template error wall: {}",
        config_out
    );
    assert!(
        config_out.lines().count() < 6,
        "error should be short, got {} lines: {}",
        config_out.lines().count(),
        config_out
    );

    // Doctor must run to completion and diagnose, not crash.
    let doctor_out = repo.jjq_failure(&["doctor"]);
    assert!(
        doctor_out.contains("cannot read jjq metadata"),
        "doctor should report the unreadable metadata: {}",
        doctor_out
    );
    assert!(
        doctor_out.contains("failure(s)"),
        "doctor should reach its summary: {}",
        doctor_out
    );
}

#[test]
fn test_doctor_fix_repairs_conflicted_metadata_bookmark() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // A real post-push metadata commit carries last_id=1; it must win.
    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "feature content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);
    repo.jjq_success(&["push", "feature"]);
    let winner = run_jj(
        repo.path(),
        &["log", "-r", "bookmarks(exact:\"jjq/_/_\")", "--no-graph", "-T", "commit_id"],
    )
    .trim()
    .to_string();

    // A metadata-like commit outside the winner's history with a lower
    // sequence counter, as a racing writer's losing side would be.
    run_jj(repo.path(), &["new", "-m", "losing meta side", "root()"]);
    fs::write(repo.path().join("last_id"), "0").unwrap();
    let loser = run_jj(repo.path(), &["log", "-r", "@", "--no-graph", "-T", "commit_id"])
        .trim()
        .to_string();
    run_jj(repo.path(), &["new", "main"]);

    // One more metadata write so the bookmark's pre-conflict target differs
    // from both sides (a side equal to the base would auto-resolve).
    repo.jjq_success(&["config", "check_command", "true"]);

    conflict_meta_bookmark(&repo, &loser, &winner);

    // Doctor detects the conflicted metadata bookmark and points at --fix.
    let doctor_out = repo.jjq_failure(&["doctor"]);
    assert!(
        doctor_out.contains("conflicted"),
        "doctor should flag the conflicted bookmark: {}",
        doctor_out
    );
    assert!(
        doctor_out.contains("jjq doctor --fix"),
        "doctor should point at --fix: {}",
        doctor_out
    );

    // --fix repairs the bookmark and the subsequent checks pass.
    let fix_out = repo.jjq_success(&["doctor", "--fix"]);
    assert!(
        fix_out.contains("all checks passed"),
        "checks should pass after repair: {}",
        fix_out
    );

    // Config reads work again.
    let trunk = repo.jjq_success(&["config", "trunk_bookmark"]);
    assert_eq!(trunk.trim(), "main", "config read should work after repair");

    // The side with the highest last_id was kept, so no IDs are reused.
    let resolved = run_jj(
        repo.path(),
        &["log", "-r", "bookmarks(exact:\"jjq/_/_\")", "--no-graph", "-T", "commit_id ++ \"\\n\""],
    );
    assert_eq!(
        resolved.trim(),
        winner,
        "bookmark should point at the side with the highest last_id"
    );
    let last_id = run_jj(
        repo.path(),
        &["file", "show", "last_id", "-r", "bookmarks(exact:\"jjq/_/_\")"],
    );
    assert_eq!(last_id.trim(), "1", "sequence counter should keep the highest value");

    // The losing side was abandoned.
    let visible = run_jj(
        repo.path(),
        &["log", "-r", "all()", "--no-graph", "-T", "commit_id ++ \"\\n\""],
    );
    assert!(
        !visible.contains(&loser),
        "losing side should be abandoned: {}",
        loser
    );
}

#[test]
fn test_doctor_fix_refuses_without_readable_last_id() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Conflict sides pointing at ordinary project commits: no side carries a
    // sequence counter, so there is nothing safe for --fix to pick.
    let side_a = run_jj(repo.path(), &["log", "-r", "main", "--no-graph", "-T", "commit_id"]);
    run_jj(repo.path(), &["new", "-m", "divergent side", "main"]);
    let side_b = run_jj(repo.path(), &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    conflict_meta_bookmark(&repo, side_a.trim(), side_b.trim());

    let out = repo.jjq_failure(&["doctor", "--fix"]);
    assert!(
        out.contains("cannot repair"),
        "--fix should refuse when no side has a readable last_id: {}",
        out
    );
    assert!(
        out.contains("bookmark set"),
        "--fix refusal should give manual repair instructions: {}",
        out
    );

    // The bookmark is untouched: still conflicted with both sides.
    let sides = run_jj(
        repo.path(),
        &["log", "-r", "bookmarks(exact:\"jjq/_/_\")", "--no-graph", "-T", "commit_id ++ \"\\n\""],
    );
    let sides: Vec<&str> = sides.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(sides.len(), 2, "bookmark should remain conflicted: {:?}", sides);
    assert!(
        sides.contains(&side_a.trim()) && sides.contains(&side_b.trim()),
        "both sides should survive a refused repair: {:?}",
        sides
    );
}

/// A check command that signals it has started by creating `started`, then
/// blocks until `release` exists. The wait is bounded so an aborted test
/// cannot leave the child running forever.
fn gated_check_command(started: &Path, release: &Path) -> String {
    format!(
        "touch {} && for i in $(seq 1 200); do [ -f {} ] && exit 0; sleep 0.1; done; exit 1",
        started.display(),
        release.display()
    )
}

/// Wait for a file to appear, panicking after `timeout`.
fn wait_for_file(path: &Path, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Set up a repo with one pushed item whose check blocks on the gate files,
/// returning (repo, sync_dir); the gate files are `started`/`release` inside
/// sync_dir.
fn repo_with_gated_check() -> (TestRepo, TempDir) {
    let repo = TestRepo::with_go_project();
    let sync = TempDir::new().expect("failed to create sync dir");
    let check = gated_check_command(&sync.path().join("started"), &sync.path().join("release"));
    repo.init_jjq_with_check(&check);

    run_jj(repo.path(), &["new", "-m", "slow feature", "main"]);
    fs::write(repo.path().join("slow.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "slow"]);
    repo.jjq_success(&["push", "slow"]);

    (repo, sync)
}

#[test]
fn test_delete_during_run_refused() {
    let (repo, sync) = repo_with_gated_check();

    let child = repo.spawn_jjq(&["run"]);
    wait_for_file(&sync.path().join("started"), std::time::Duration::from_secs(30));

    // The runner has claimed the item and is inside the check; deleting it
    // now would be silently overridden by the landing, so delete must refuse.
    let output = repo
        .jjq()
        .args(["delete", "1"])
        .output()
        .expect("failed to run jjq delete");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(10),
        "delete of an in-flight item should refuse with USAGE; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert!(
        stderr.contains("being processed"),
        "refusal should explain the item is being processed: {}",
        stderr
    );

    // Release the check; the run completes and lands the item (the delete
    // was refused, so landing it respects the user's visible state).
    fs::write(sync.path().join("release"), "").unwrap();
    let run_output = child.wait_with_output().expect("failed to wait for run");
    assert!(
        run_output.status.success(),
        "run should complete successfully; stdout: {} stderr: {}",
        String::from_utf8_lossy(&run_output.stdout),
        String::from_utf8_lossy(&run_output.stderr)
    );
    assert!(
        repo.jj_file_exists("slow.txt", "main"),
        "item should have landed on main"
    );
}

#[test]
fn test_delete_stale_running_claim() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "interrupted feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);

    // Manufacture a stale claim, as a runner crash mid-processing leaves it.
    // No run is active, so the run lock is free.
    run_jj(
        repo.path(),
        &["bookmark", "create", "-r", "feature", "jjq/running/000001"],
    );

    let output = repo.jjq_success(&["delete", "1"]);
    assert!(
        output.contains("deleted"),
        "delete should remove the stale claim: {}",
        output
    );

    let bookmarks = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/running/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    assert!(
        bookmarks.trim().is_empty(),
        "stale running bookmark should be gone: {}",
        bookmarks
    );
}

#[test]
fn test_run_recovers_stale_claim() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "interrupted feature", "main"]);
    fs::write(repo.path().join("feature.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "feature"]);
    repo.jjq_success(&["push", "feature"]);

    // Simulate a runner that crashed mid-check: the queue bookmark became
    // jjq/running/000001, the jjq-run-000001 workspace stayed registered,
    // and the process died. Recovery must clear the workspace too, or
    // re-claiming the item dies on the name collision.
    run_jj(
        repo.path(),
        &[
            "bookmark",
            "create",
            "-r",
            "jjq/queue/000001",
            "jjq/running/000001",
        ],
    );
    run_jj(repo.path(), &["bookmark", "delete", "jjq/queue/000001"]);
    let crashed_ws = TempDir::new().expect("failed to create temp dir");
    run_jj(
        repo.path(),
        &[
            "workspace",
            "add",
            "-r",
            "jjq/running/000001",
            "--name",
            "jjq-run-000001",
            crashed_ws.path().to_str().unwrap(),
        ],
    );

    let output = repo.jjq_success(&["run"]);
    assert!(
        output.contains("recovered interrupted item 1"),
        "run should recover the orphaned claim: {}",
        output
    );
    assert!(
        repo.jj_file_exists("feature.txt", "main"),
        "recovered item should land on main"
    );

    let bookmarks = run_jj(
        repo.path(),
        &[
            "bookmark",
            "list",
            "-r",
            "bookmarks(glob:\"jjq/*/??????\")",
            "-T",
            "name ++ \"\\n\"",
        ],
    );
    assert!(
        bookmarks.trim().is_empty(),
        "no queue/running/failed bookmarks should remain: {}",
        bookmarks
    );
}

#[test]
fn test_clean_refused_during_run() {
    let (repo, sync) = repo_with_gated_check();

    let child = repo.spawn_jjq(&["run"]);
    wait_for_file(&sync.path().join("started"), std::time::Duration::from_secs(30));

    // The runner's workspace is live; clean must refuse rather than destroy
    // it out from under the check process.
    let output = repo
        .jjq()
        .args(["clean"])
        .output()
        .expect("failed to run jjq clean");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "clean during a run should exit CONFLICT; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert!(
        stderr.contains("queue runner is active"),
        "refusal should name the active runner: {}",
        stderr
    );

    // The run completes untouched.
    fs::write(sync.path().join("release"), "").unwrap();
    let run_output = child.wait_with_output().expect("failed to wait for run");
    assert!(
        run_output.status.success(),
        "run should complete successfully after refused clean; stdout: {} stderr: {}",
        String::from_utf8_lossy(&run_output.stdout),
        String::from_utf8_lossy(&run_output.stderr)
    );
}

#[test]
fn test_clean_spares_user_jjq_prefixed_workspace() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // A user workspace whose name merely starts with "jjq" must survive.
    let user_ws = TempDir::new().unwrap();
    run_jj(
        repo.path(),
        &[
            "workspace",
            "add",
            "--name",
            "jjq-experiments",
            user_ws.path().to_str().unwrap(),
        ],
    );

    // A manufactured stale runner workspace matching the generated pattern
    // must be cleaned.
    let stale_ws = TempDir::new().unwrap();
    run_jj(
        repo.path(),
        &[
            "workspace",
            "add",
            "--name",
            "jjq-run-000042",
            stale_ws.path().to_str().unwrap(),
        ],
    );

    let output = repo.jjq_success(&["clean"]);
    assert!(
        output.contains("jjq-run-000042"),
        "stale runner workspace should be cleaned: {}",
        output
    );

    let list = run_jj(repo.path(), &["workspace", "list"]);
    assert!(
        list.contains("jjq-experiments"),
        "user workspace should survive clean: {}",
        list
    );
    assert!(
        !list.contains("jjq-run-000042"),
        "stale runner workspace should be forgotten: {}",
        list
    );
}

#[test]
fn test_status_shows_running_item_during_run() {
    let (repo, sync) = repo_with_gated_check();

    let child = repo.spawn_jjq(&["run"]);
    wait_for_file(&sync.path().join("started"), std::time::Duration::from_secs(30));

    let (stdout, _stderr, success) = repo.jjq_raw_output(&["status", "--json"]);
    assert!(success, "status --json should succeed during a run");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --json should return valid JSON");
    assert_eq!(
        parsed["runner_active"], true,
        "runner_active should be true during a run: {}",
        stdout
    );
    let running = parsed["running"]
        .as_array()
        .expect("running should be an array");
    assert_eq!(running.len(), 1, "one item should be running: {}", stdout);
    assert_eq!(running[0]["id"], 1);
    assert_eq!(running[0]["description"], "slow feature");

    let text = repo.jjq_success(&["status"]);
    assert!(
        text.contains("Running:"),
        "text status should show a Running section: {}",
        text
    );

    fs::write(sync.path().join("release"), "").unwrap();
    let run_output = child.wait_with_output().expect("failed to wait for run");
    assert!(
        run_output.status.success(),
        "run should complete; stderr: {}",
        String::from_utf8_lossy(&run_output.stderr)
    );
}

#[test]
fn test_doctor_warns_on_bare_symbol_log_filter() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    // Old jjq versions wrote the filter as a bare bookmark symbol, which
    // breaks every jj command's short-prefixes index the moment the bookmark
    // becomes conflicted.
    run_jj(
        repo.path(),
        &["config", "set", "--repo", "revsets.log", "~ ::jjq/_/_"],
    );

    let out = repo.jjq_success(&["doctor"]);
    assert!(
        out.contains("WARN"),
        "doctor should warn about the old filter form: {}",
        out
    );
    assert!(
        out.contains("jj config set --repo revsets.log '~ ::bookmarks(exact:\"jjq/_/_\")'"),
        "migration hint should show the exact command: {}",
        out
    );
}
