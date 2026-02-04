// ABOUTME: Executes the check command as a child process, capturing output to a log file.
// ABOUTME: Provides heartbeat progress output for non-interactive (CI) environments.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::runlog;

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

    // Wait loop: behaviour depends on whether stderr is a TTY.
    let status = if std::io::stderr().is_terminal() {
        // Interactive path (placeholder -- Task 5 will add the real spinner).
        wait_non_interactive(&mut child)?
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
