# Progress Indicators & Output Viewing Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add liveness feedback (spinner + elapsed time) during check command execution, interactive output toggle via keypress, and a `jjq tail` command for viewing check output from a separate terminal.

**Architecture:** Replace the blocking `Command::output()` call with `Command::spawn()`. A reader thread pipes child output to `.jj/jjq-run.log`. The main thread renders a spinner (TTY) or heartbeat (non-TTY) and handles keypress toggling. A new `jjq tail` command reads from the same log file.

**Tech Stack:** Rust std library (`std::io::IsTerminal`, `std::process::Stdio`), `libc` (termios for raw mode), `ctrlc` (signal-safe terminal restore). No TUI crates.

---

### Task 1: Add Dependencies

**Files:**
- Modify: `Cargo.toml:10-17`

**Step 1: Add libc and ctrlc to Cargo.toml**

Add to `[dependencies]`:

```toml
libc = "0.2"
ctrlc = "2"
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors

**Step 3: Commit**

```
jj desc -m "Add libc and ctrlc dependencies for progress indicators"
```

---

### Task 2: Create `src/runlog.rs` — Log File Path & Sentinel

This module owns the log file path derivation and sentinel constants. Both `runner.rs` and `tail.rs` will depend on it.

**Files:**
- Create: `src/runlog.rs`
- Modify: `src/main.rs:4-9` (add `mod runlog;`)

**Step 1: Write the test**

Add to `tests/e2e.rs`:

```rust
#[test]
fn test_run_creates_log_file() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq();

    run_jj(repo.path(), &["new", "-m", "test feature", "main"]);
    fs::write(repo.path().join("newfile.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "simple-branch"]);
    repo.jjq_success(&["push", "simple-branch"]);

    // Run should create a log file
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_run_creates_log_file -- --nocapture`
Expected: FAIL — log file does not exist yet

**Step 3: Write `src/runlog.rs`**

```rust
// ABOUTME: Log file path derivation and sentinel constants for check command output.
// ABOUTME: Shared between the runner (writer) and the tail command (reader).

use anyhow::Result;
use std::path::PathBuf;

use crate::jj;

/// Sentinel line written to the log file when a check command completes.
pub const SENTINEL_PREFIX: &str = "--- jjq: run complete";

/// Build the sentinel line for a given exit code.
pub fn sentinel_line(exit_code: i32) -> String {
    format!("--- jjq: run complete (exit {}) ---", exit_code)
}

/// Get the path to the run log file for the current repository.
pub fn log_path() -> Result<PathBuf> {
    let root = jj::repo_root()?;
    Ok(root.join(".jj").join("jjq-run.log"))
}
```

**Step 4: Add `mod runlog;` to `src/main.rs`**

Add `mod runlog;` after the existing module declarations (line 9).

**Step 5: Verify it compiles**

Run: `cargo check`
Expected: compiles (test still fails — that's expected, we haven't wired it up yet)

**Step 6: Commit**

```
jj desc -m "Add runlog module with log path and sentinel constants"
```

---

### Task 3: Create `src/runner.rs` — Non-Interactive Path

The core process execution engine. This task implements only the non-interactive (non-TTY) path: spawn the child, pipe output to the log file via a reader thread, print heartbeat lines every 15 seconds.

**Files:**
- Create: `src/runner.rs`
- Modify: `src/main.rs:4-9` (add `mod runner;`)

**Step 1: Write `src/runner.rs`**

```rust
// ABOUTME: Process execution engine for check commands with output capture.
// ABOUTME: Handles spawning, log file writing, spinner display, and keypress toggling.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::runlog;

/// Run a check command, capturing output to a log file and displaying progress.
///
/// - `command`: the shell command to run (executed via `sh -c`)
/// - `log_path`: path to write the output log (truncated on entry)
///
/// Returns the child's exit status.
pub fn run_check_command(command: &str, log_path: &Path) -> Result<ExitStatus> {
    // Ensure parent directory exists
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Truncate and open the log file
    let log_file = File::create(log_path)
        .with_context(|| format!("failed to create log file: {}", log_path.display()))?;

    // Merge stderr into stdout at the shell level
    let wrapped = format!("{} 2>&1", command);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&wrapped)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn check command")?;

    let stdout = child.stdout.take().expect("stdout was piped");

    // Reader thread: pipe child stdout to log file
    let mut log_writer = io::BufWriter::new(log_file);
    let reader_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let _ = writeln!(log_writer, "{}", line);
                    let _ = log_writer.flush();
                }
                Err(_) => break,
            }
        }
    });

    let interactive = io::stderr().is_terminal();

    if interactive {
        wait_interactive(&mut child)?;
    } else {
        wait_non_interactive(&mut child)?;
    }

    let status = child.wait().context("failed to wait for check command")?;

    // Wait for reader thread to finish
    let _ = reader_handle.join();

    // Write sentinel to log file
    let mut sentinel_file = fs::OpenOptions::new()
        .append(true)
        .open(log_path)
        .context("failed to open log file for sentinel")?;
    writeln!(sentinel_file, "{}", runlog::sentinel_line(status.code().unwrap_or(-1)))?;

    Ok(status)
}

/// Non-interactive wait: heartbeat every 15 seconds.
fn wait_non_interactive(child: &mut std::process::Child) -> Result<()> {
    let start = Instant::now();
    let heartbeat_interval = Duration::from_secs(15);
    let mut last_heartbeat = start;

    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None => {
                let now = Instant::now();
                if now.duration_since(last_heartbeat) >= heartbeat_interval {
                    let elapsed = now.duration_since(start);
                    eprintln!("jjq: still running... (elapsed: {})", format_duration(elapsed));
                    last_heartbeat = now;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Interactive wait: spinner + elapsed time (placeholder for Task 4).
fn wait_interactive(child: &mut std::process::Child) -> Result<()> {
    // For now, just use non-interactive path. Task 4 adds the spinner.
    wait_non_interactive(child)
}

/// Format a duration as human-readable (e.g., "1m 23s", "45s").
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}
```

**Step 2: Add `mod runner;` to `src/main.rs`**

Add after the other mod declarations.

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors

**Step 4: Commit**

```
jj desc -m "Add runner module with non-interactive check command execution"
```

---

### Task 4: Wire Runner Into `commands.rs`

Replace the `Command::output()` calls in `run_one()` and `check()` with `runner::run_check_command()`.

**Files:**
- Modify: `src/commands.rs:571-575` (`run_one` check execution)
- Modify: `src/commands.rs:691-701` (`check` command execution)

**Step 1: Modify `run_one()` in `commands.rs`**

Replace lines 571-575:

```rust
let check_output = Command::new("sh").arg("-c").arg(&check_command).output()?;

if !check_output.status.success() {
    eprintln!("{}", String::from_utf8_lossy(&check_output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&check_output.stderr));
```

With:

```rust
let log_path = crate::runlog::log_path()?;
let check_status = crate::runner::run_check_command(&check_command, &log_path)?;

if !check_status.success() {
    // Print captured output from log file on failure
    if let Ok(log_contents) = std::fs::read_to_string(&log_path) {
        // Skip the sentinel line at the end
        for line in log_contents.lines() {
            if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                eprintln!("{}", line);
            }
        }
    }
```

Also update the success check further down — replace `check_output.status.success()` references with `check_status.success()`.

**Step 2: Modify `check()` in `commands.rs`**

Replace lines 691-701:

```rust
let check_output = Command::new("sh").arg("-c").arg(&check_command).output()?;

let stdout = String::from_utf8_lossy(&check_output.stdout);
let stderr = String::from_utf8_lossy(&check_output.stderr);

if !stdout.is_empty() {
    print!("{}", stdout);
}
if !stderr.is_empty() {
    eprint!("{}", stderr);
}

let success = check_output.status.success();
```

With:

```rust
let log_path = crate::runlog::log_path()?;
let check_status = crate::runner::run_check_command(&check_command, &log_path)?;

// Print captured output from log file
if let Ok(log_contents) = std::fs::read_to_string(&log_path) {
    for line in log_contents.lines() {
        if !line.starts_with(crate::runlog::SENTINEL_PREFIX) {
            println!("{}", line);
        }
    }
}

let success = check_status.success();
```

**Step 3: Remove unused `Command` import if no longer needed**

Check if `std::process::Command` is still used elsewhere in `commands.rs`. If not, remove it from the imports at line 8.

**Step 4: Run all tests**

Run: `cargo test`
Expected: all existing tests pass. The `test_run_creates_log_file` test from Task 2 should now also pass.

**Step 5: Run the e2e test script**

Run: `./jjq-test`
Expected: passes

**Step 6: Commit**

```
jj desc -m "Wire runner module into run_one and check commands"
```

---

### Task 5: Add Spinner + Elapsed Time (Interactive Path)

Replace the placeholder `wait_interactive` in `runner.rs` with a real spinner that updates at ~100ms using `\r` on stderr.

**Files:**
- Modify: `src/runner.rs` (replace `wait_interactive`)

**Step 1: Write `wait_interactive` with spinner**

Replace the placeholder `wait_interactive` function:

```rust
/// Interactive wait: spinner + elapsed time on stderr.
fn wait_interactive(child: &mut std::process::Child) -> Result<()> {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let start = Instant::now();
    let mut frame_idx: usize = 0;

    loop {
        match child.try_wait()? {
            Some(_) => {
                // Clear the spinner line
                eprint!("\r\x1b[2K");
                return Ok(());
            }
            None => {
                let elapsed = start.elapsed();
                let frame = FRAMES[frame_idx % FRAMES.len()];
                eprint!(
                    "\r\x1b[2Kjjq: running check {} {}",
                    frame,
                    format_duration(elapsed)
                );
                let _ = io::stderr().flush();
                frame_idx += 1;
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
```

**Step 2: Run all tests**

Run: `cargo test`
Expected: all tests pass (tests run in non-TTY mode, so they hit the non-interactive path)

**Step 3: Manual test**

Run in a real terminal with a slow check command:
```
jjq config check_command "sleep 5"
jjq push @-
jjq run
```
Expected: see a spinner with elapsed time that updates in place, then clears on completion.

**Step 4: Commit**

```
jj desc -m "Add spinner with elapsed time for interactive check command execution"
```

---

### Task 6: Add Keypress Toggle (Press `v` to View Output)

Add raw mode terminal handling and keypress detection to the interactive wait loop. Pressing `v` toggles between spinner mode and output streaming mode.

**Files:**
- Modify: `src/runner.rs` (extend `wait_interactive`, add raw mode helpers)

**Step 1: Add raw mode helpers**

Add to `runner.rs`:

```rust
/// RAII guard that restores terminal settings on drop.
struct RawMode {
    original: libc::termios,
    fd: i32,
}

impl RawMode {
    /// Enter raw mode on stdin. Returns None if stdin is not a terminal.
    fn enter() -> Option<Self> {
        if !io::stdin().is_terminal() {
            return None;
        }
        unsafe {
            let fd = libc::STDIN_FILENO;
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut original) != 0 {
                return None;
            }
            let mut raw = original;
            // Disable canonical mode and echo
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            // Read returns after 0 bytes (non-blocking with VMIN=0, VTIME=0)
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { original, fd })
        }
    }

    /// Try to read a single byte from stdin (non-blocking).
    fn try_read_byte(&self) -> Option<u8> {
        let mut buf = [0u8; 1];
        let n = unsafe {
            libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, 1)
        };
        if n == 1 { Some(buf[0]) } else { None }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}
```

**Step 2: Update `wait_interactive` to handle keypress toggle**

The function needs access to the log path for tailing. Update the signature of `run_check_command` to pass `log_path` through, and update `wait_interactive`:

```rust
fn wait_interactive(child: &mut std::process::Child, log_path: &Path) -> Result<()> {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let start = Instant::now();
    let mut frame_idx: usize = 0;
    let mut streaming = false;
    let mut log_read_pos: u64 = 0;

    let raw_mode = RawMode::enter();

    loop {
        // Check for keypress
        if let Some(ref rm) = raw_mode {
            if let Some(b) = rm.try_read_byte() {
                if b == b'v' {
                    streaming = !streaming;
                    if streaming {
                        // Clear spinner, show header, dump last 20 lines
                        eprint!("\r\x1b[2K");
                        eprintln!("jjq: --- check output (press v to hide) ---");
                        show_tail_lines(log_path, 20);
                        // Set read position to end of file for streaming
                        log_read_pos = fs::metadata(log_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                    } else {
                        eprintln!("jjq: --- output hidden ---");
                    }
                }
            }
        }

        // Check if child exited
        match child.try_wait()? {
            Some(_) => {
                if streaming {
                    // Flush remaining output
                    stream_from_pos(log_path, &mut log_read_pos);
                } else {
                    eprint!("\r\x1b[2K");
                }
                return Ok(());
            }
            None => {}
        }

        if streaming {
            stream_from_pos(log_path, &mut log_read_pos);
        } else {
            let elapsed = start.elapsed();
            let frame = FRAMES[frame_idx % FRAMES.len()];
            eprint!(
                "\r\x1b[2Kjjq: running check {} {}",
                frame,
                format_duration(elapsed)
            );
            let _ = io::stderr().flush();
            frame_idx += 1;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

/// Print the last N lines of a file to stderr.
fn show_tail_lines(path: &Path, n: usize) {
    let Ok(contents) = fs::read_to_string(path) else { return };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        // Skip sentinel lines
        if !line.starts_with(runlog::SENTINEL_PREFIX) {
            eprintln!("{}", line);
        }
    }
}

/// Stream new content from `path` starting at `pos`, updating `pos`.
fn stream_from_pos(path: &Path, pos: &mut u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = File::open(path) else { return };
    let Ok(metadata) = file.metadata() else { return };
    if metadata.len() <= *pos {
        return;
    }
    let _ = file.seek(SeekFrom::Start(*pos));
    let mut buf = String::new();
    let Ok(n) = file.read_to_string(&mut buf) else { return };
    if n > 0 {
        for line in buf.lines() {
            if !line.starts_with(runlog::SENTINEL_PREFIX) {
                eprintln!("{}", line);
            }
        }
        *pos += n as u64;
    }
}
```

**Step 3: Set up ctrlc handler for terminal restore**

At the top of `run_check_command`, before entering raw mode, set up a ctrlc handler. Since the `Drop` guard on `RawMode` handles normal exits, the ctrlc handler needs to restore terminal state and re-raise the signal:

```rust
// Store original termios for signal handler
let original_termios = unsafe {
    let mut t: libc::termios = std::mem::zeroed();
    libc::tcgetattr(libc::STDIN_FILENO, &mut t);
    t
};

ctrlc::set_handler(move || {
    // Restore terminal
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original_termios);
    }
    // Clear spinner line
    eprint!("\r\x1b[2K");
    std::process::exit(130); // Standard SIGINT exit code
})?;
```

Place this setup inside `run_check_command` only when `interactive` is true, before the `wait_interactive` call.

**Step 4: Run all tests**

Run: `cargo test`
Expected: all tests pass

**Step 5: Manual test**

In a real terminal:
```
jjq config check_command "for i in $(seq 1 10); do echo line $i; sleep 1; done"
jjq push @-
jjq run
```
- Press `v` during the spinner → should see output lines appearing
- Press `v` again → should hide output and resume spinner
- Press Ctrl+C → terminal should be restored properly

**Step 6: Commit**

```
jj desc -m "Add keypress toggle for live output viewing during check execution"
```

---

### Task 7: Create `src/tail.rs` and `Tail` Subcommand

**Files:**
- Create: `src/tail.rs`
- Modify: `src/main.rs:21-82` (add `Tail` variant to `Commands` enum)
- Modify: `src/main.rs:95-112` (add match arm in `run()`)

**Step 1: Write the test**

Add to `tests/e2e.rs`:

```rust
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
```

**Step 2: Run tests to verify they fail**

Run: `cargo test test_tail -- --nocapture`
Expected: FAIL — `Tail` variant doesn't exist yet

**Step 3: Write `src/tail.rs`**

```rust
// ABOUTME: Implementation of `jjq tail` for viewing check command output.
// ABOUTME: Reads from the shared log file, supports following and dump modes.

use anyhow::Result;
use std::fs::{self, File};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};

use crate::lock;
use crate::runlog;

/// Run the tail command.
///
/// - `all`: if true, show from beginning; otherwise show last 20 lines
/// - `follow`: if true, follow the file for new output (default behavior)
pub fn tail(all: bool, follow: bool) -> Result<()> {
    let log_path = runlog::log_path()?;

    if !log_path.exists() {
        eprintln!("jjq: no run output available");
        return Ok(());
    }

    if !follow {
        // Dump mode: print content and exit
        let contents = fs::read_to_string(&log_path)?;
        let lines: Vec<&str> = contents.lines().collect();
        let start = if all { 0 } else { lines.len().saturating_sub(20) };
        for line in &lines[start..] {
            if !line.starts_with(runlog::SENTINEL_PREFIX) {
                println!("{}", line);
            }
        }
        return Ok(());
    }

    // Follow mode
    let mut file = File::open(&log_path)?;

    if !all {
        // Seek to show last 20 lines, then follow
        let contents = fs::read_to_string(&log_path)?;
        let lines: Vec<&str> = contents.lines().collect();
        let start = lines.len().saturating_sub(20);
        for line in &lines[start..] {
            if line.starts_with(runlog::SENTINEL_PREFIX) {
                println!("{}", line);
                return Ok(());
            }
            println!("{}", line);
        }
        // Seek to end for following
        file.seek(SeekFrom::End(0))?;
    } else {
        // Print everything from the beginning, checking for sentinel
        let reader = io::BufReader::new(&file);
        for line in reader.lines() {
            let line = line?;
            if line.starts_with(runlog::SENTINEL_PREFIX) {
                println!("{}", line);
                return Ok(());
            }
            println!("{}", line);
        }
    }

    // Follow loop: poll for new content
    let poll_interval = std::time::Duration::from_millis(200);
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = file.read_to_string(&mut buf)?;
        if n > 0 {
            for line in buf.lines() {
                println!("{}", line);
                let _ = io::stdout().flush();
                if line.starts_with(runlog::SENTINEL_PREFIX) {
                    return Ok(());
                }
            }
        } else {
            // No new data — check if runner is still alive
            if !lock::is_held("run")? {
                // Runner is not running and no sentinel seen — crashed or finished
                eprintln!("jjq: run process is no longer active");
                return Ok(());
            }
            std::thread::sleep(poll_interval);
        }
    }
}
```

**Step 4: Add `Tail` to `Commands` enum in `src/main.rs`**

Add after the `Check` variant (around line 54):

```rust
    /// View check command output
    Tail {
        /// Show output from the beginning (default: last 20 lines)
        #[arg(long)]
        all: bool,
        /// Don't follow output, just dump and exit
        #[arg(long)]
        no_follow: bool,
    },
```

**Step 5: Add match arm in `run()` function in `src/main.rs`**

Add after the `Check` match arm (around line 105):

```rust
        Commands::Tail { all, no_follow } => tail::tail(all, !no_follow),
```

Note: the flag is `--no-follow` but we pass `follow = !no_follow` to the function.

**Step 6: Add `mod tail;` to `src/main.rs`**

Add after the other mod declarations.

**Step 7: Run all tests**

Run: `cargo test`
Expected: all tests pass, including the new `test_tail_*` tests

**Step 8: Commit**

```
jj desc -m "Add jjq tail command for viewing check command output"
```

---

### Task 8: Update Existing Snapshot Tests

The change from `Command::output()` to the runner will cause some snapshot tests to change — specifically, the failure output for `test_run_check_failure` may differ slightly because output is now printed line-by-line from the log file rather than dumped from memory. The blank lines from empty stdout/stderr will disappear.

**Files:**
- Modify: `tests/e2e.rs` (update snapshots as needed)

**Step 1: Run all tests and update snapshots**

Run: `cargo test`

If any insta snapshots fail, review the diffs. The expected changes are:
- The two empty lines in `test_run_check_failure` (from empty stdout and stderr being printed) may disappear since `false` produces no output and the log file will be empty except for the sentinel.

**Step 2: Update snapshots**

Run: `cargo insta review`

Verify each snapshot change makes sense. Accept if the output is correct for the new behavior.

**Step 3: Run full CI**

Run: `make ci`
Expected: all checks pass

**Step 4: Commit**

```
jj desc -m "Update test snapshots for runner-based check execution"
```

---

### Task 9: Test Edge Cases

**Files:**
- Modify: `tests/e2e.rs` (add edge case tests)

**Step 1: Add test for check command with output on failure**

```rust
#[test]
fn test_run_failure_shows_output() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_with_check("echo 'build failed: missing dependency' && exit 1");

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
```

**Step 2: Add test for log file truncation between runs**

```rust
#[test]
fn test_log_file_truncated_between_runs() {
    let repo = TestRepo::with_go_project();
    repo.init_jjq_with_check("echo run-marker-$RANDOM");

    // First run
    run_jj(repo.path(), &["new", "-m", "feature 1", "main"]);
    fs::write(repo.path().join("f1.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f1"]);
    repo.jjq_success(&["push", "f1"]);
    repo.jjq_success(&["run"]);

    let log_path = repo.path().join(".jj").join("jjq-run.log");
    let first_contents = fs::read_to_string(&log_path).unwrap();

    // Second run
    run_jj(repo.path(), &["new", "-m", "feature 2", "main"]);
    fs::write(repo.path().join("f2.txt"), "content").unwrap();
    run_jj(repo.path(), &["bookmark", "create", "f2"]);
    repo.jjq_success(&["push", "f2"]);
    repo.jjq_success(&["run"]);

    let second_contents = fs::read_to_string(&log_path).unwrap();

    // Log should only contain one sentinel (file was truncated)
    let sentinel_count = second_contents.matches(runlog::SENTINEL_PREFIX).count();
    assert_eq!(
        sentinel_count, 1,
        "log should contain exactly one sentinel after truncation, got {}: {}",
        sentinel_count, second_contents
    );
}
```

Note: This test imports `runlog` — since it's an e2e test using `assert_cmd`, it won't have access to the `runlog` module directly. Instead, use the string literal:

```rust
    let sentinel_count = second_contents.matches("--- jjq: run complete").count();
```

**Step 3: Add test for tail --all**

```rust
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
```

**Step 4: Run all tests**

Run: `cargo test`
Expected: all pass

**Step 5: Run full CI**

Run: `make ci`
Expected: all pass

**Step 6: Commit**

```
jj desc -m "Add edge case tests for runner and tail"
```
