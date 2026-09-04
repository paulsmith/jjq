// ABOUTME: Executes the check command as a child process, capturing output to a log file.
// ABOUTME: Provides spinner progress and keypress-toggled live output for interactive terminals.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Read as _, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use std::os::unix::process::CommandExt;

use crate::runlog;

/// Braille spinner frames for the interactive wait loop.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Format a duration as human-readable elapsed time.
/// Under 60 seconds: "Xs", otherwise "Nm Xs".
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// RAII guard that puts stdin into raw mode and restores on drop.
/// Returns None if stdin is not a terminal.
struct RawMode {
    original: libc::termios,
    fd: i32,
}

impl RawMode {
    fn enter() -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let fd = libc::STDIN_FILENO;
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut original) != 0 {
                return None;
            }
            let mut raw = original;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { original, fd })
        }
    }

    /// Non-blocking read of a single byte from stdin.
    fn try_read_byte(&self) -> Option<u8> {
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), 1) };
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

/// Print the last `n` lines from a log file to stderr, skipping sentinel lines.
fn show_tail_lines(path: &Path, n: usize) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = contents
        .lines()
        .filter(|l| !l.starts_with(runlog::SENTINEL_PREFIX))
        .collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        eprintln!("{}", line);
    }
}

/// Read new content from `path` starting at byte offset `pos`, print lines to
/// stderr (skipping sentinel lines), and update `pos` to the new end.
fn stream_from_pos(path: &Path, pos: &mut u64) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    if file.seek(SeekFrom::Start(*pos)).is_err() {
        return;
    }
    let mut buf = String::new();
    if let Ok(n) = file.read_to_string(&mut buf) {
        for line in buf.lines() {
            if !line.starts_with(runlog::SENTINEL_PREFIX) {
                eprintln!("{}", line);
            }
        }
        *pos += n as u64;
    }
}

/// Run a check command with `working_dir` as its working directory, logging
/// its merged stdout+stderr to `log_path`.
///
/// The working directory is set on the child process only — the jjq process
/// itself never changes directory. Returns the child's exit status. A
/// sentinel line is appended to the log after the child exits regardless of
/// success or failure.
pub fn run_check_command(command: &str, working_dir: &Path, log_path: &Path) -> Result<ExitStatus> {
    run_check_command_with_mode(
        command,
        working_dir,
        log_path,
        std::io::stderr().is_terminal(),
        false,
        None,
        None,
    )
    .map(|result| result.status)
}

struct BackgroundStart {
    gate: std::path::PathBuf,
    cancel_marker: std::path::PathBuf,
    parent_pid: u32,
    process_identity: String,
}

/// Run a check without touching terminal state. Parallel queue workers use
/// this: raw mode, the global Ctrl-C handler, and a shared spinner are only
/// meaningful for one foreground check.
pub fn run_check_command_noninteractive<F>(
    command: &str,
    working_dir: &Path,
    log_path: &Path,
    start_gate: &Path,
    cancel_marker: &Path,
    parent_pid: u32,
    process_identity: &str,
    on_spawn: F,
) -> Result<BackgroundCheckResult>
where
    F: FnOnce(u32) -> Result<bool> + Send + 'static,
{
    run_check_command_with_mode(
        command,
        working_dir,
        log_path,
        false,
        true,
        Some(BackgroundStart {
            gate: start_gate.to_path_buf(),
            cancel_marker: cancel_marker.to_path_buf(),
            parent_pid,
            process_identity: process_identity.to_string(),
        }),
        Some(Box::new(on_spawn)),
    )
}

/// Run a serial queue check with normal foreground progress feedback while
/// retaining the token-scoped cancellation/start handshake.
pub fn run_check_command_cancellable<F>(
    command: &str,
    working_dir: &Path,
    log_path: &Path,
    start_gate: &Path,
    cancel_marker: &Path,
    parent_pid: u32,
    process_identity: &str,
    on_spawn: F,
) -> Result<BackgroundCheckResult>
where
    F: FnOnce(u32) -> Result<bool> + Send + 'static,
{
    run_check_command_with_mode(
        command,
        working_dir,
        log_path,
        std::io::stderr().is_terminal(),
        false,
        Some(BackgroundStart {
            gate: start_gate.to_path_buf(),
            cancel_marker: cancel_marker.to_path_buf(),
            parent_pid,
            process_identity: process_identity.to_string(),
        }),
        Some(Box::new(on_spawn)),
    )
}

pub struct BackgroundCheckResult {
    pub status: ExitStatus,
    pub cancelled: bool,
    /// The supervisor gave up waiting for its start gate (or its parent)
    /// before the user command ran, so `status` is protocol noise, not a
    /// verdict on the candidate.
    pub not_started: bool,
}

fn run_check_command_with_mode(
    command: &str,
    working_dir: &Path,
    log_path: &Path,
    interactive: bool,
    silent: bool,
    background_start: Option<BackgroundStart>,
    on_spawn: Option<Box<dyn FnOnce(u32) -> Result<bool> + Send>>,
) -> Result<BackgroundCheckResult> {
    // Ensure parent directories exist.
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating log directory {}", parent.display()))?;
    }

    // Truncate/create the log file.
    let log_file = File::create(log_path)
        .with_context(|| format!("creating log file {}", log_path.display()))?;

    // Background checks start behind a gate. Gate path, parent PID, process
    // identity, and user command are positional arguments, never interpolated
    // into supervisor shell source. The supervisor drops a `<gate>.started`
    // marker just before running the user command, which is how the parent
    // tells a genuine exit status from a supervisor that gave up waiting.
    let mut child_command = Command::new("sh");
    if let Some(start) = &background_start {
        child_command
            .arg("-c")
            .arg("gate=$1; parent=$2; identity=$3; user_command=$4; cancel=$5; deadline=$(($(date +%s) + 4)); while [ ! -f \"$gate\" ]; do [ -f \"$cancel\" ] && exit 77; kill -0 \"$parent\" 2>/dev/null || exit 75; [ \"$(date +%s)\" -lt \"$deadline\" ] || exit 76; sleep 0.02; done; [ -f \"$cancel\" ] && exit 77; : \"$identity\"; : > \"$gate.started\"; sh -c \"$user_command\" 2>&1 & check=$!; while kill -0 \"$check\" 2>/dev/null; do if [ -f \"$cancel\" ] || ! kill -0 \"$parent\" 2>/dev/null; then kill -TERM \"$check\" 2>/dev/null; sleep 1; kill -KILL -$$ 2>/dev/null; exit 77; fi; sleep 1; done; wait \"$check\"")
            .arg("jjq-check-supervisor")
            .arg(&start.gate)
            .arg(start.parent_pid.to_string())
            .arg(&start.process_identity)
            .arg(command)
            .arg(&start.cancel_marker);
    } else {
        child_command.arg("-c").arg(format!("{} 2>&1", command));
    }
    if background_start.is_some() {
        unsafe {
            child_command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = child_command
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning check command: {}", command))?;

    if background_start.is_some()
        && let Err(error) = background_spawned_test_hook(child.id())
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let prestart_cancelled = if let Some(on_spawn) = on_spawn {
        match on_spawn(child.id()) {
            Ok(cancelled) => cancelled,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("publishing background check process");
            }
        }
    } else {
        false
    };
    if !prestart_cancelled
        && let Some(start) = &background_start
        && let Err(error) = open_start_gate(&start.gate)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("opening background check start gate");
    }

    // Take the child's stdout pipe for the reader thread.
    let child_stdout = child.stdout.take().expect("child stdout was piped");

    // Spawn a reader thread that writes each line to the log file.
    let mut log_writer = log_file;
    let reader_handle = thread::spawn(move || -> Result<()> {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines() {
            let line = line.context("reading child stdout")?;
            writeln!(log_writer, "{}", line).context("writing to log file")?;
            log_writer.flush().context("flushing log file")?;
        }
        Ok(())
    });

    // Set up ctrlc handler to restore terminal before exiting.
    if interactive {
        let original_termios = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(libc::STDIN_FILENO, &mut t);
            t
        };
        // ctrlc::set_handler can only be called once per process; subsequent
        // calls are silently ignored (ctrlc v2 returns () regardless).
        ctrlc::set_handler(move || {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original_termios);
            }
            eprint!("\r\x1b[2K");
            std::process::exit(130);
        });
    }

    // Wait loop: behaviour depends on whether stderr is a TTY.
    let cancel_marker = background_start.as_ref().map(|start| start.cancel_marker.as_path());
    let mut result = if interactive {
        wait_interactive(&mut child, log_path, cancel_marker)?
    } else if silent && let Some(start) = &background_start {
        wait_background(&mut child, &start.cancel_marker)?
    } else if silent {
        BackgroundCheckResult {
            status: child.wait().context("waiting for background check process")?,
            cancelled: false,
            not_started: false,
        }
    } else {
        wait_non_interactive(&mut child, cancel_marker)?
    };
    if let Some(start) = &background_start {
        result.not_started = !result.cancelled && !started_marker(&start.gate).exists();
        remove_start_gate(&start.gate);
    }

    // Join the reader thread and propagate any I/O errors.
    reader_handle
        .join()
        .expect("reader thread panicked")?;

    // Append sentinel line.
    let exit_code = result.status.code().unwrap_or(-1);
    let mut log_append = OpenOptions::new()
        .append(true)
        .open(log_path)
        .with_context(|| format!("reopening log file {}", log_path.display()))?;
    writeln!(log_append, "{}", runlog::sentinel_line(exit_code))
        .context("writing sentinel line")?;

    Ok(result)
}

/// Wait for a token-scoped background check. The owning thread is the only
/// code that signals the child process group, so a stale PID in repository
/// state can never target an unrelated process.
fn wait_background(
    child: &mut std::process::Child,
    cancel_marker: &Path,
) -> Result<BackgroundCheckResult> {
    let poll_interval = Duration::from_millis(50);

    loop {
        // A completed command wins over a concurrently arriving intent. This
        // is only the worker result; the coordinator still serializes the
        // durable terminal outcome under publication.lock.
        if let Some(status) = child.try_wait().context("polling background check process")? {
            return Ok(BackgroundCheckResult {
                // 77 is an internal pre-start supervisor protocol only when
                // the durable intent exists. The configured user command is
                // otherwise free to return 77 as an ordinary check failure.
                cancelled: status.code() == Some(77) && cancel_marker.exists(),
                status,
                not_started: false,
            });
        }

        if cancel_marker.exists() {
            let status = terminate_process_group(child)?;
            return Ok(BackgroundCheckResult {
                status,
                cancelled: true,
                not_started: false,
            });
        }

        thread::sleep(poll_interval);
    }
}

fn terminate_process_group(child: &mut std::process::Child) -> Result<ExitStatus> {
    let process_group = -(child.id() as i32);
    let term_result = unsafe { libc::kill(process_group, libc::SIGTERM) };
    if term_result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("terminating check process group");
        }
    }

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut leader_status = None;
    loop {
        if leader_status.is_none() {
            leader_status = child.try_wait().context("waiting for terminated check")?;
        }
        let group_exists = unsafe { libc::kill(process_group, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !group_exists {
            return match leader_status {
                Some(status) => Ok(status),
                None => child.wait().context("reaping terminated check process"),
            };
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let kill_result = unsafe { libc::kill(process_group, libc::SIGKILL) };
    if kill_result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("killing check process group");
        }
    }
    match leader_status {
        Some(status) => Ok(status),
        None => child.wait().context("reaping killed check process"),
    }
}

fn open_start_gate(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    file.sync_all()?;
    Ok(())
}

/// Marker the supervisor creates immediately before the user command runs.
fn started_marker(gate: &Path) -> std::path::PathBuf {
    gate.with_extension("started")
}

fn remove_start_gate(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(started_marker(path));
    if let Some(parent) = path.parent()
        && fs::read_dir(parent)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
    {
        let _ = fs::remove_dir(parent);
    }
}

/// Deterministic test seam for the spawn-to-publication crash window.
fn background_spawned_test_hook(child_pid: u32) -> Result<()> {
    let Ok(started) = std::env::var("JJQTEST_BACKGROUND_SPAWNED") else {
        return Ok(());
    };
    fs::write(started, child_pid.to_string())?;
    let Some(release) = std::env::var_os("JJQTEST_BACKGROUND_SPAWN_RELEASE") else {
        return Ok(());
    };
    let release = std::path::PathBuf::from(release);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for background spawn test release");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Non-interactive wait: poll every second, emit heartbeat every 15 seconds.
fn wait_non_interactive(
    child: &mut std::process::Child,
    cancel_marker: Option<&Path>,
) -> Result<BackgroundCheckResult> {
    let start = Instant::now();
    let mut last_heartbeat = Instant::now();
    let heartbeat_interval = std::env::var("JJQTEST_HEARTBEAT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(15));
    let poll_interval = if cancel_marker.is_some() {
        Duration::from_millis(100)
    } else {
        Duration::from_secs(1)
    };

    loop {
        if let Some(status) = child.try_wait().context("polling child process")? {
            return Ok(BackgroundCheckResult { status, cancelled: false, not_started: false });
        }

        if cancel_marker.is_some_and(Path::exists) {
            return Ok(BackgroundCheckResult {
                status: terminate_process_group(child)?,
                cancelled: true,
                not_started: false,
            });
        }

        thread::sleep(poll_interval);

        if last_heartbeat.elapsed() >= heartbeat_interval {
            let elapsed = format_duration(start.elapsed());
            eprintln!("jjq: still running... (elapsed: {})", elapsed);
            last_heartbeat = Instant::now();
        }
    }
}

/// Interactive wait: show a spinner with elapsed time, allow pressing `v` to
/// toggle live output streaming from the log file.
fn wait_interactive(
    child: &mut std::process::Child,
    log_path: &Path,
    cancel_marker: Option<&Path>,
) -> Result<BackgroundCheckResult> {
    let start = Instant::now();
    let poll_interval = Duration::from_millis(100);
    let mut frame_idx: usize = 0;
    let mut streaming = false;
    let mut stream_pos: u64 = 0;

    let _raw_mode = RawMode::enter();

    loop {
        // Check for keypress.
        if let Some(ref raw) = _raw_mode
            && let Some(b'v') = raw.try_read_byte()
        {
            streaming = !streaming;
            if streaming {
                // Clear spinner line, print header, show tail context.
                eprint!("\r\x1b[2K");
                eprintln!("jjq: --- check output (press v to hide) ---");
                show_tail_lines(log_path, 20);
                // Set stream position to current end of file.
                stream_pos = fs::metadata(log_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
            } else {
                eprintln!("jjq: --- output hidden ---");
            }
        }

        // Check if child exited.
        if let Some(status) = child.try_wait().context("polling child process")? {
            if streaming {
                // Flush remaining output.
                stream_from_pos(log_path, &mut stream_pos);
            } else {
                // Clear spinner line.
                eprint!("\r\x1b[2K");
            }
            return Ok(BackgroundCheckResult { status, cancelled: false, not_started: false });
        }

        if cancel_marker.is_some_and(Path::exists) {
            eprint!("\r\x1b[2K");
            return Ok(BackgroundCheckResult {
                status: terminate_process_group(child)?,
                cancelled: true,
                not_started: false,
            });
        }

        if streaming {
            stream_from_pos(log_path, &mut stream_pos);
        } else {
            let elapsed = format_duration(start.elapsed());
            let spinner = SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()];
            eprint!("\r\x1b[2Kjjq: running check {} {}", spinner, elapsed);
            let _ = std::io::stderr().flush();
            frame_idx += 1;
        }

        thread::sleep(poll_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "61m 1s");
    }

    #[test]
    fn foreground_check_stays_in_callers_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_path = temp.path().join("pid");
        let release_path = temp.path().join("release");
        let log_path = temp.path().join("check.log");
        let command = format!(
            "echo $$ > {}; while [ ! -f {} ]; do sleep 0.02; done",
            pid_path.display(),
            release_path.display()
        );
        let working_dir = temp.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            run_check_command(&command, &working_dir, &log_path).unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() {
            assert!(Instant::now() < deadline, "foreground check did not start");
            thread::sleep(Duration::from_millis(10));
        }
        let pid: libc::pid_t = fs::read_to_string(&pid_path).unwrap().trim().parse().unwrap();
        assert_eq!(unsafe { libc::getpgid(pid) }, unsafe { libc::getpgrp() });
        fs::write(release_path, "").unwrap();
        assert!(handle.join().unwrap().success());
    }
}
