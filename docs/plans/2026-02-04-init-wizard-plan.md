# Init Wizard Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace silent auto-initialization with an explicit `jjq init` wizard that prompts for trunk bookmark and check command, then runs doctor.

**Architecture:** Add `Init` CLI variant with optional `--trunk`/`--check` flags. Split `ensure_initialized()` into `is_initialized()` (already exists) and `initialize()`. Guard all other commands with an `is_initialized()` check that errors if false. Interactive prompts read from stdin with TTY detection.

**Tech Stack:** Rust, clap, std::io (IsTerminal, BufRead)

---

### Task 1: Add `jj::list_bookmarks()` helper

We need a function that returns all bookmark names (for auto-detecting trunk).

**Files:**
- Modify: `src/jj.rs`

**Step 1: Write the helper**

Add this function to `src/jj.rs` after the existing `bookmark_list_glob` function (after line 111):

```rust
/// List all local bookmark names.
pub fn list_bookmarks() -> Result<Vec<String>> {
    let output = run_ok(&["bookmark", "list", "-T", "name ++ \"\\n\""])?;
    Ok(output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: compiles with no errors

**Step 3: Commit**

```
jj desc -m "Add list_bookmarks helper to jj module"
jj new
```

---

### Task 2: Add `Init` CLI variant to `main.rs`

**Files:**
- Modify: `src/main.rs:22-64` (the `Commands` enum and `run()` match)

**Step 1: Add the Init variant**

Add this variant to the `Commands` enum, before `Push`:

```rust
    /// Initialize jjq in this repository
    Init {
        /// Trunk bookmark name
        #[arg(long)]
        trunk: Option<String>,
        /// Check command
        #[arg(long)]
        check: Option<String>,
    },
```

**Step 2: Add the match arm**

In the `run()` function's match block, add before the `Push` arm:

```rust
        Commands::Init { trunk, check } => commands::init(trunk.as_deref(), check.as_deref()),
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: compile error — `commands::init` doesn't exist yet. That's fine, we'll add it next.

**Step 4: Commit**

```
jj desc -m "Add Init command variant to CLI"
jj new
```

---

### Task 3: Implement `commands::init()` — the wizard

This is the main task. The function needs to:
1. Check if already initialized → error
2. Prompt for trunk (with auto-detected default) if `--trunk` not provided
3. Prompt for check command if `--check` not provided
4. Detect non-TTY and error if required args missing
5. Call `config::initialize()` to create metadata branch
6. Set the config values
7. Run doctor

**Files:**
- Modify: `src/commands.rs`

**Step 1: Add the init function**

Add this function to `src/commands.rs` (before `push`):

```rust
/// Initialize jjq in this repository.
pub fn init(trunk: Option<&str>, check: Option<&str>) -> Result<()> {
    use std::io::{self, BufRead, IsTerminal, Write};

    // Refuse if already initialized
    if config::is_initialized()? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "jjq is already initialized. Use 'jjq config' to change settings.",
        )
        .into());
    }

    println!("Initializing jjq in this repository.");
    println!();

    let is_tty = io::stdin().is_terminal();

    // Determine trunk bookmark
    let trunk_value = if let Some(t) = trunk {
        t.to_string()
    } else if !is_tty {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "--trunk and --check are required in non-interactive mode.",
        )
        .into());
    } else {
        // Auto-detect default from existing bookmarks
        let bookmarks = jj::list_bookmarks().unwrap_or_default();
        let default = if bookmarks.iter().any(|b| b == "main") {
            Some("main")
        } else if bookmarks.iter().any(|b| b == "master") {
            Some("master")
        } else {
            None
        };

        prompt_with_default("Trunk bookmark", default)?
    };

    // Determine check command
    let check_value = if let Some(c) = check {
        c.to_string()
    } else if !is_tty {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "--trunk and --check are required in non-interactive mode.",
        )
        .into());
    } else {
        prompt_required("Check command", "A check command is required (e.g., 'make test', 'cargo test').")?
    };

    // Initialize metadata branch
    config::initialize()?;

    // Set config values
    config::set("trunk_bookmark", &trunk_value)?;
    config::set("check_command", &check_value)?;

    println!();
    println!("Initialized jjq:");
    println!("  trunk_bookmark = {}", trunk_value);
    println!("  check_command  = {}", check_value);
    println!();

    // Run doctor
    println!("Running doctor...");
    let _ = doctor();

    println!();
    println!("Ready to go! Queue revisions with 'jjq push <revset>'.");

    Ok(())
}

/// Prompt for a value with an optional default. Loops until non-empty input.
fn prompt_with_default(label: &str, default: Option<&str>) -> Result<String> {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        if let Some(d) = default {
            print!("{} [{}]: ", label, d);
        } else {
            print!("{}: ", label);
        }
        io::stdout().flush()?;

        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            if let Some(d) = default {
                return Ok(d.to_string());
            }
            // No default, re-prompt
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

/// Prompt for a required value (no default). Loops until non-empty input, showing hint on empty.
fn prompt_required(label: &str, hint: &str) -> Result<String> {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        print!("{}: ", label);
        io::stdout().flush()?;

        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            println!("{}", hint);
            continue;
        }
        return Ok(trimmed.to_string());
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: compile error — `config::initialize` doesn't exist as a separate public function yet. We'll fix that in Task 4.

**Step 3: Commit**

```
jj desc -m "Implement init command with interactive wizard"
jj new
```

---

### Task 4: Split `config::ensure_initialized()` into `initialize()`

Extract the body of `ensure_initialized()` into a new public `initialize()` function. Keep `ensure_initialized()` but have it call `initialize()`.

**Files:**
- Modify: `src/config.rs:28-59`

**Step 1: Extract initialize()**

Replace the existing `ensure_initialized()` with:

```rust
/// Create the jjq metadata branch. Errors if already initialized.
pub fn initialize() -> Result<()> {
    // Create new revision parented to root()
    let change_id = jj::new_rev(&["root()"])?;
    jj::run_quiet(&["bookmark", "create", "-r", &change_id, JJQ_BOOKMARK])?;

    // Create workspace to set up initial state
    let temp_dir = TempDir::new()?;
    let workspace_name = format!("jjq{}", std::process::id());

    jj::workspace_add(
        temp_dir.path().to_str().unwrap(),
        &workspace_name,
        &[JJQ_BOOKMARK],
    )?;

    // Change to workspace and create last_id file
    let orig_dir = env::current_dir()?;
    env::set_current_dir(temp_dir.path())?;

    fs::write("last_id", "0")?;
    jj::describe("@", "init jjq")?;
    jj::run_quiet(&["squash"])?;

    env::set_current_dir(&orig_dir)?;
    jj::workspace_forget(&workspace_name)?;

    Ok(())
}

/// Ensure jjq is initialized, creating metadata branch if needed.
pub fn ensure_initialized() -> Result<()> {
    if is_initialized()? {
        return Ok(());
    }
    initialize()
}
```

**Step 2: Verify it compiles and tests pass**

Run: `cargo build && cargo test`
Expected: compiles. Some existing tests may now fail because they relied on auto-init. We'll fix those in Task 6.

**Step 3: Commit**

```
jj desc -m "Extract config::initialize() from ensure_initialized()"
jj new
```

---

### Task 5: Guard all commands with initialization check

Replace `ensure_initialized()` calls with a check that errors if not initialized. The key change: commands that currently silently auto-initialize should now fail with a message telling the user to run `jjq init`.

**Files:**
- Modify: `src/commands.rs`

**Step 1: Add a require_initialized helper**

Add this helper near the top of `commands.rs` (after `preferr`):

```rust
/// Require jjq to be initialized, or error with instructions.
fn require_initialized() -> Result<()> {
    if !config::is_initialized()? {
        return Err(ExitError::new(
            exit_codes::USAGE,
            "jjq is not initialized. Run 'jjq init' first.",
        )
        .into());
    }
    Ok(())
}
```

**Step 2: Replace ensure_initialized() calls**

Find every call to `config::ensure_initialized()` in `commands.rs` and replace it with `require_initialized()`. There are calls in:

- `push()` at line 178: `config::ensure_initialized()?;` → `require_initialized()?;`
- `run_one()` at line 260: `config::ensure_initialized()?;` → `require_initialized()?;`
- `delete()` at line 526: `config::ensure_initialized()?;` → `require_initialized()?;`
- `config()` function at line 569: `config::ensure_initialized()?;` → `require_initialized()?;`

**Step 3: Add require_initialized to commands that read config without ensure_initialized**

Several commands read config without `ensure_initialized`. Add `require_initialized()` at the top of:

- `check()` — reads check_command via `config::get_check_command()` (line 405, add before the resolve_revset call)
- `clean()` — already tolerates uninitialized, keep as-is (it just looks at workspaces)
- `doctor()` — already handles uninitialized state gracefully, keep as-is
- `status()` — already checks `is_initialized()` and prints a message, but update the message

**Step 4: Update status() uninitialized message**

In `status()` (line 474), change the message from:
```rust
prefout("jjq not initialized (run 'jjq push <revset>' to start)");
```
to:
```rust
prefout("jjq not initialized. Run 'jjq init' first.");
```

**Step 5: Update doctor() uninitialized message**

In `doctor()` (line 643), change:
```rust
print_check("WARN", "jjq not initialized (run 'jjq push' to initialize)");
```
to:
```rust
print_check("FAIL", "jjq not initialized (run 'jjq init')");
fails += 1;
```
Remove the `warns += 1;` line and add `fails += 1;` since this is now a hard error.

**Step 6: Update doctor() check command hint**

In `doctor()` (line 666), change the hint:
```rust
print_hint("to fix: jjq config check_command '<command>'");
```
to:
```rust
print_hint("to fix: jjq config check_command '<command>' (or re-run 'jjq init')");
```

**Step 7: Update check_command error messages in run_one()**

In `run_one()` (line 269), update the error message:
```rust
preferr("check_command not configured (use 'jjq config check_command <cmd>')");
```
to:
```rust
preferr("check_command not configured (use 'jjq config check_command <cmd>' or re-run 'jjq init')");
```

**Step 8: Verify it compiles**

Run: `cargo build`
Expected: compiles. Tests will break — we fix those in Task 6.

**Step 9: Commit**

```
jj desc -m "Guard commands with require_initialized, replace auto-init"
jj new
```

---

### Task 6: Update existing tests for init-first workflow

All existing tests that currently rely on auto-initialization via `push` or `config set` need to call `jjq init` first.

**Files:**
- Modify: `tests/e2e.rs`

**Step 1: Add init helper to TestRepo**

Add this method to `TestRepo`:

```rust
    /// Initialize jjq with default settings.
    fn init_jjq(&self) {
        self.jjq_success(&["init", "--trunk", "main", "--check", "true"]);
    }

    /// Initialize jjq with a specific check command.
    fn init_jjq_with_check(&self, check_cmd: &str) {
        self.jjq_success(&["init", "--trunk", "main", "--check", check_cmd]);
    }
```

**Step 2: Update each test**

Go through each test and add initialization. The pattern is: any test that uses `push`, `run`, `config set`, `delete`, or `check` now needs `init` first. Tests that explicitly test uninitialized behavior (`test_status_uninitialized`) should be updated to reference `jjq init`.

Here are the specific changes:

- `test_status_uninitialized` — update snapshot to new message: `"jjq: jjq not initialized. Run 'jjq init' first."`

- `test_push_no_trunk` — this test pushes to a repo without a `main` bookmark. Now it should fail earlier with "not initialized". Change to test that push without init errors. Or: init with `--trunk nonexistent --check true`, then push should fail with trunk not found. Let's do the latter to keep testing the trunk-not-found error:
  ```rust
  fn test_push_no_trunk() {
      let repo = TestRepo::new();
      fs::write(repo.path().join("file.txt"), "content").unwrap();
      run_jj(repo.path(), &["desc", "-m", "test commit"]);
      // Init with a trunk that doesn't exist
      repo.jjq_success(&["init", "--trunk", "main", "--check", "true"]);
      let output = repo.jjq_failure(&["push", "@"]);
      insta::assert_snapshot!(output, @"jjq: trunk bookmark 'main' not found");
  }
  ```

- `test_config_show_all` — add `repo.init_jjq();` instead of using push to initialize. Update snapshot (check_command will now be "true" from init, not "(not set)").

- `test_config_set_and_get` — add `repo.init_jjq();` instead of push.

- `test_config_invalid_key` — add `repo.init_jjq();` instead of push.

- `test_push_and_status` — add `repo.init_jjq();` before push.

- `test_push_conflict_detection` — add `repo.init_jjq();` before the pushes.

- `test_run_empty_queue` — replace `config set` with `repo.init_jjq();`.

- `test_run_no_check_command` — this tests missing check command. Now init requires check command, so this test scenario changes. Either: init with a check command, then unset it via some other means (not possible via config), OR skip this test since init now prevents this state. Simplest: remove this test since the init wizard prevents this case. Or: keep as a test that push without init fails.

- `test_run_success` — replace `config set` with `repo.init_jjq();`.

- `test_run_check_failure` — replace `config set` with `repo.init_jjq_with_check("false");`.

- `test_run_all` — replace `config set` with `repo.init_jjq();`.

- `test_delete_queued` — add `repo.init_jjq();` before push.

- `test_delete_failed` — replace `config set` with `repo.init_jjq_with_check("false");`.

- `test_delete_not_found` — replace push-to-initialize with `repo.init_jjq();`.

- `test_sequence_id_validation` — replace push-to-initialize with `repo.init_jjq();`, then push main.

- `test_full_workflow_with_prs` — replace `config set` with `repo.init_jjq_with_check("make");`.

- `test_multiple_push_same_revision` — add `repo.init_jjq();`.

- `test_log_hint_not_shown_in_non_tty` — add `repo.init_jjq();`.

- `test_log_hint_shown_once_when_forced` — add `repo.init_jjq();`.

- `test_run_all_stop_on_failure_flag` — replace `config set` with `repo.init_jjq();`.

- `test_run_all_continues_on_failure` — replace `config set` with `repo.init_jjq();`.

- `test_run_all_partial_failure_exit_code` — replace `config set` with `repo.init_jjq();`.

- `test_log_hint_skipped_when_filter_configured` — add `repo.init_jjq();`.

- `test_push_exact_duplicate_rejected` — add `repo.init_jjq();`.

- `test_push_idempotent_clears_failed` — replace `config set` with `repo.init_jjq_with_check("false");`.

- `test_clean_no_workspaces` — keep as-is (clean doesn't require init).

- `test_clean_removes_failed_workspaces` — replace `config set` with `repo.init_jjq_with_check("false");`.

**Step 3: Run tests and update snapshots**

Run: `cargo test`
Expected: Some snapshot mismatches. Review them and accept with `cargo insta review` or update inline.

**Step 4: Commit**

```
jj desc -m "Update all e2e tests for init-first workflow"
jj new
```

---

### Task 7: Add new tests for `jjq init`

**Files:**
- Modify: `tests/e2e.rs`

**Step 1: Write init-specific tests**

Add these tests:

```rust
#[test]
fn test_init_with_flags() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_success(&["init", "--trunk", "main", "--check", "make test"]);
    assert!(output.contains("Initialized jjq"), "should show init confirmation: {}", output);
    assert!(output.contains("trunk_bookmark = main"), "should show trunk: {}", output);
    assert!(output.contains("check_command  = make test"), "should show check cmd: {}", output);
    assert!(output.contains("Ready to go!"), "should show ready message: {}", output);
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
    assert!(output.contains("jj repository"), "should run doctor: {}", output);
    assert!(output.contains("ok"), "doctor checks should pass: {}", output);
}

#[test]
fn test_push_without_init_fails() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_failure(&["push", "main"]);
    insta::assert_snapshot!(output, @"jjq: jjq is not initialized. Run 'jjq init' first.");
}

#[test]
fn test_run_without_init_fails() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_failure(&["run"]);
    insta::assert_snapshot!(output, @"jjq: jjq is not initialized. Run 'jjq init' first.");
}

#[test]
fn test_check_without_init_fails() {
    let repo = TestRepo::with_go_project();
    let output = repo.jjq_failure(&["check"]);
    insta::assert_snapshot!(output, @"jjq: jjq is not initialized. Run 'jjq init' first.");
}
```

**Step 2: Run the new tests**

Run: `cargo test`
Expected: all new tests pass

**Step 3: Commit**

```
jj desc -m "Add e2e tests for init command"
jj new
```

---

### Task 8: Update README.md

**Files:**
- Modify: `README.md`

**Step 1: Update the Usage section**

Add an "Initialize" subsection before "Push a revision to the queue":

```markdown
### Initialize

Set up jjq in your repository:

```sh
jjq init
```

Or non-interactively:

```sh
jjq init --trunk main --check "make test"
```
```

**Step 2: Update the Configure section**

Change the configure section to note that `jjq init` handles initial setup, and `jjq config` is for changing settings after init:

```markdown
### Configure

After initialization, change settings with:

```sh
jjq config                           # show all config
jjq config check_command "make test" # change check command
jjq config trunk_bookmark main       # change trunk bookmark name
jjq config max_failures 5            # set max failures shown in status
```
```

**Step 3: Update the Configuration table**

Change `check_command` description from `*(none — must be set before first run)*` to `*(set during init)*`.

**Step 4: Commit**

```
jj desc -m "Update README for init command"
jj new
```

---

### Task 9: Update man page (`docs/jjq.1`)

**Files:**
- Modify: `docs/jjq.1`

**Step 1: Add init to SYNOPSIS**

Add at the top of the SYNOPSIS (before `.B jjq push`):

```nroff
.B jjq init
.RB [ \-\-trunk
.IR bookmark ]
.RB [ \-\-check
.IR command ]
.br
```

**Step 2: Add init command section**

Add before the `push` command section (before `.SS push`):

```nroff
.SS init \fR[\fB\-\-trunk \fIbookmark\fR] [\fB\-\-check \fIcommand\fR]
Initialize jjq in the current repository.
Sets up the metadata branch and configures the trunk bookmark and check
command.
.PP
With
.B \-\-trunk
and
.B \-\-check
flags, runs non\-interactively.
Without flags, prompts for each value.
The trunk bookmark defaults to
.B main
if that bookmark exists, or
.B master
as a fallback.
.PP
If stdin is not a terminal and required flags are missing, exits with
an error.
.PP
Refuses to run if jjq is already initialized.
Use
.B jjq config
to change settings after initialization.
.PP
After configuration, runs
.B jjq doctor
to validate the setup.
```

**Step 3: Update TYPICAL WORKFLOW**

Change "Initialize a project" from:
```nroff
jjq config trunk_bookmark main
jjq config check_command "make test && make lint"
```
to:
```nroff
jjq init --trunk main --check "make test && make lint"
```

**Step 4: Update check_command CONFIGURATION note**

Change the note about configuring before running push/run to reference init:
```nroff
Must be set during
.B jjq init
or configured afterward with
.BR "jjq config" .
```

**Step 5: Commit**

```
jj desc -m "Update man page for init command"
jj new
```

---

### Task 10: Regenerate `docs/jjq.1.txt`

**Files:**
- Modify: `docs/jjq.1.txt`

**Step 1: Regenerate the plain text version**

Run: `MANWIDTH=78 man -l docs/jjq.1 | col -bx > docs/jjq.1.txt`

If that doesn't work (macOS may not support `-l`), try:
```
groff -man -Tutf8 docs/jjq.1 | col -bx > docs/jjq.1.txt
```

Or manually: `nroff -man docs/jjq.1 | col -bx > docs/jjq.1.txt`

**Step 2: Verify the output looks right**

Read `docs/jjq.1.txt` and verify the init section is present.

**Step 3: Commit**

```
jj desc -m "Regenerate jjq.1.txt"
jj new
```

---

### Task 11: Update AGENTS.md

**Files:**
- Modify: `AGENTS.md`

**Step 1: Add init to the Commands table**

Add to the Commands table (before `push`):

```markdown
| `init [--trunk <bookmark>] [--check <cmd>]` | Initialize jjq, set trunk and check command |
```

**Step 2: Update "Key Concepts" if needed**

No changes needed — the existing concepts still apply.

**Step 3: Update any references to auto-initialization**

Search for mentions of "push to initialize" or similar and update to reference `jjq init`.

**Step 4: Commit**

```
jj desc -m "Update AGENTS.md for init command"
jj new
```

---

### Task 12: Final verification

**Step 1: Run full test suite**

Run: `cargo test`
Expected: all tests pass

**Step 2: Run clippy**

Run: `cargo clippy`
Expected: no warnings

**Step 3: Build release**

Run: `cargo build --release`
Expected: builds successfully

**Step 4: Manual smoke test**

Create a temp jj repo and test the init flow:
```sh
d=$(mktemp -d)
cd "$d"
jj git init .
echo "test" > file.txt
jj desc -m "initial"
jj bookmark create main
/path/to/jjq init --trunk main --check "echo ok"
/path/to/jjq status
```

**Step 5: Squash commits**

Squash all the task commits into a clean set:
```
jj squash -m "Add jjq init wizard"
```
