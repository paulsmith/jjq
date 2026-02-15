// ABOUTME: Implements the `jjq tail` subcommand for viewing check command output.
// ABOUTME: Supports dump mode and follow mode with poll-based file tailing.

use anyhow::Result;
use std::io::{self, Write};

/// View check command output, optionally following new output in real time.
///
/// In dump mode (`!follow`), prints existing log content and exits.
/// In follow mode, prints initial content then polls for new lines until
/// a sentinel or runner exit is detected.
pub fn tail(all: bool, follow: bool) -> Result<()> {
    let path = crate::runlog::log_path()?;

    if !path.exists() {
        eprintln!("jjq: no run output available");
        return Ok(());
    }

    let content = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = content.lines().collect();

    if !follow {
        // Dump mode: print content and exit.
        let visible: Vec<&str> = lines
            .iter()
            .filter(|l| !l.starts_with(crate::runlog::SENTINEL_PREFIX))
            .copied()
            .collect();
        let start = if all || visible.len() <= 20 {
            0
        } else {
            visible.len() - 20
        };
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for line in &visible[start..] {
            writeln!(out, "{}", line)?;
        }
        return Ok(());
    }

    // Follow mode: print initial content, then poll for new lines.
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let visible: Vec<&str> = lines
        .iter()
        .filter(|l| !l.starts_with(crate::runlog::SENTINEL_PREFIX))
        .copied()
        .collect();
    let already_finished = lines
        .iter()
        .any(|l| l.starts_with(crate::runlog::SENTINEL_PREFIX));
    let start = if all || visible.len() <= 20 {
        0
    } else {
        visible.len() - 20
    };
    for line in &visible[start..] {
        writeln!(out, "{}", line)?;
    }
    if already_finished {
        return Ok(());
    }
    out.flush()?;

    // Track our read position by byte offset.
    let mut offset = content.len();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));

        let current = std::fs::read_to_string(&path)?;
        if current.len() > offset {
            let new_data = &current[offset..];
            for line in new_data.lines() {
                if line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                    return Ok(());
                }
                writeln!(out, "{}", line)?;
            }
            out.flush()?;
            offset = current.len();
        } else if !crate::lock::is_held("run")? {
            eprintln!("jjq: run process is no longer active");
            return Ok(());
        }
    }
}
