# Progress Indicators & Output Viewing Design

## Goal

Provide liveness feedback during long-running check commands and give users
a way to view check output without disrupting `jjq run`.

Primary driver: **human experience** — check commands can take 60+ seconds
with no visible feedback.

## Changes

### 1. Output Capture & Log File

Replace the blocking `Command::output()` call with `Command::spawn()` and
piped stdout/stderr. A reader thread copies the child's output to a log
file at `<repo-root>/.jj/jjq-run.log`.

The child command is wrapped as `sh -c "<command> 2>&1"` to merge stderr
into stdout at the shell level. This means one pipe, one reader thread, no
mutex on the log file.

The log file is truncated at the start of each `run_one()` call. When the
check finishes, a sentinel line is written:

```
--- jjq: run complete (exit N) ---
```

The log file is preserved after the run completes so it can be reviewed.

### 2. Spinner & Elapsed Time

While the check command runs, `jjq run` displays a liveness indicator on
stderr. Behavior adapts to the terminal context.

**Interactive (TTY on stderr):**

A single line updated in-place via `\r`:

```
jjq: running check ⠹ 1m 23s
```

Braille spinner characters (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) cycle at ~100ms. Elapsed
time updates every second. The line is replaced with the final result on
completion.

**Non-interactive (piped/captured):**

A heartbeat line every 15 seconds:

```
jjq: still running... (elapsed: 15s)
jjq: still running... (elapsed: 30s)
```

No carriage returns, no ANSI codes.

TTY detection uses `std::io::stderr().is_terminal()` (stable since Rust
1.70).

### 3. Keypress Toggle

In interactive mode, pressing `v` toggles live output viewing on and off.

**Toggle on:**

1. Spinner line is cleared.
2. Separator: `jjq: --- check output (press v to hide) ---`
3. Last 20 lines from the log file are printed as context.
4. New output streams to stderr in real time (main thread tails the log
   file).

**Toggle off:**

1. Separator: `jjq: --- output hidden ---`
2. Spinner resumes.

Output scrolls the terminal naturally — no alternate screen buffer or
ANSI region management.

**Terminal raw mode:** stdin is put into raw mode via `libc` termios calls
to detect keypresses without blocking. A `Drop` guard restores the
original terminal state. A `ctrlc` signal handler also restores terminal
state before exiting, preventing a broken terminal on Ctrl+C.

Toggle state resets to "off" at the start of each queue item.

### 4. `jjq tail` Command

A separate command to view check output from another terminal.

```
jjq tail              # last 20 lines + follow
jjq tail --all        # from beginning + follow
jjq tail --no-follow  # dump and exit
```

Reads from `.jj/jjq-run.log`. Follows by polling at 200ms. Exits when it
sees the sentinel line.

**Crash detection:** if the run lock (`.jj/jjq-locks/`) is free but no
sentinel has appeared, the runner crashed. `jjq tail` detects this and
exits with a message rather than polling forever.

If no log file exists, prints `jjq: no run output available` and exits.

No IPC between `jjq run` and `jjq tail` — just a shared file. Multiple
`jjq tail` processes can read concurrently.

## Architecture

### New Files

- **`src/runner.rs`** — Process execution engine. Spawns the child, pipes
  output to the log file via a reader thread, manages the spinner, handles
  keypress toggling and raw mode. Single entry point:
  `run_check_command(command: &str, log_path: &Path) -> Result<ExitStatus>`.
  The spinner is a private helper inside this module.

- **`src/tail.rs`** — The `jjq tail` implementation. File reading,
  following, sentinel detection, crash detection via lock state.

### Modified Files

- **`src/commands.rs`** — `run_one()` and `check()` call
  `runner::run_check_command()` instead of `Command::output()`.

- **`src/main.rs`** — Add `Tail` subcommand to the `Commands` enum. Add
  `mod runner; mod tail;`.

- **`Cargo.toml`** — Add `ctrlc = "2"` and `libc = "0.2"`.

### Threading Model

```
Main thread:                    Reader thread:
  spawn child process             loop {
  spawn reader thread               read from child stdout pipe
  enter raw mode (if TTY)            write line to log file
  loop {                             (EOF: write sentinel, break)
    poll stdin (100ms timeout)     }
    if 'v' pressed:
      toggle output mode
    if spinner mode:
      update spinner line
    if streaming mode:
      tail log file, print to stderr
    if child exited:
      break
  }
  join reader thread
  leave raw mode
```

The reader thread has no shared state with the main thread. It writes to
the log file; the main thread reads from it when streaming. All stderr
output is owned by the main thread.

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| In-memory ring buffer | Not used | Log file serves the same purpose; eliminates shared state |
| Log location | `.jj/jjq-run.log` | Immune to tmpdir cleanup, trivially discoverable, consistent with lock files |
| stderr merging | `2>&1` in shell | One pipe, one thread, no mutex |
| Raw mode | `libc` + `ctrlc` crate | `libc` is already a transitive dep; `ctrlc` handles signal-safe terminal restore |
| Tail polling | 200ms file poll | Simple, portable, adequate |
| Output display | Plain scrolling | Simpler than alternate screen; `jjq tail` covers the "dedicated viewer" case |

## Implementation Order

1. `runner.rs` — non-interactive path only (spawn, pipe to log, heartbeat).
2. Refactor `commands.rs` to use `runner::run_check_command()`. Verify
   existing tests pass.
3. Add spinner + elapsed time for the interactive path.
4. Add keypress toggle with raw mode and `ctrlc` handler.
5. `tail.rs` + `Tail` subcommand in `main.rs`.
