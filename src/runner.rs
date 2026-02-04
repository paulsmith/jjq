// ABOUTME: Executes the check command as a child process, capturing output to a log file.
// ABOUTME: Provides spinner progress and keypress-toggled live output for interactive terminals.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Read as _, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

/// Run a check command, logging its merged stdout+stderr to `log_path`.
///
/// Returns the child's exit status. A sentinel line is appended to the log
/// after the child exits regardless of success or failure.
pub fn run_check_command(command: &str, log_path: &Path) -> Result<ExitStatus> {
    // Ensure parent directories exist.
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating log directory {}", parent.display()))?;
    }

    // Truncate/create the log file.
    let log_file = File::create(log_path)
        .with_context(|| format!("creating log file {}", log_path.display()))?;

    // Spawn child: sh -c "<command> 2>&1" with stdout piped.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{} 2>&1", command))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning check command: {}", command))?;

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

    let interactive = std::io::stderr().is_terminal();

    // Set up ctrlc handler to restore terminal before exiting.
    if interactive {
        let original_termios = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(libc::STDIN_FILENO, &mut t);
            t
        };
        // Ignore the error: set_handler can only be called once per process.
        #[allow(clippy::let_unit_value)]
        let _ = ctrlc::set_handler(move || {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original_termios);
            }
            eprint!("\r\x1b[2K");
            std::process::exit(130);
        });
    }

    // Wait loop: behaviour depends on whether stderr is a TTY.
    let status = if interactive {
        wait_interactive(&mut child, log_path)?
    } else {
        wait_non_interactive(&mut child)?
    };

    // Join the reader thread and propagate any I/O errors.
    reader_handle
        .join()
        .expect("reader thread panicked")?;

    // Append sentinel line.
    let exit_code = status.code().unwrap_or(-1);
    let mut log_append = OpenOptions::new()
        .append(true)
        .open(log_path)
        .with_context(|| format!("reopening log file {}", log_path.display()))?;
    writeln!(log_append, "{}", runlog::sentinel_line(exit_code))
        .context("writing sentinel line")?;

    Ok(status)
}

/// Non-interactive wait: poll every second, emit heartbeat every 15 seconds.
fn wait_non_interactive(child: &mut std::process::Child) -> Result<ExitStatus> {
    let start = Instant::now();
    let mut last_heartbeat = Instant::now();
    let heartbeat_interval = Duration::from_secs(15);
    let poll_interval = Duration::from_secs(1);

    loop {
        if let Some(status) = child.try_wait().context("polling child process")? {
            return Ok(status);
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
fn wait_interactive(child: &mut std::process::Child, log_path: &Path) -> Result<ExitStatus> {
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
            return Ok(status);
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
}
