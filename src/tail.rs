// ABOUTME: Implements the `jjq tail` subcommand for viewing check command output.
// ABOUTME: Supports dump mode and follow mode with poll-based file tailing.

use anyhow::Result;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Stable identity of an open log file across polls. The publish-by-rename
/// pattern gives a replaced log a fresh inode, which distinguishes replacement
/// from appends of any length.
fn identity_of(file: &File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|m| (m.dev(), m.ino()))
}

/// View check command output, optionally following new output in real time.
///
/// In dump mode (`!follow`), prints existing log content and exits.
/// In follow mode, prints initial content then polls for new lines until
/// a sentinel or runner exit is detected.
pub fn tail(id: Option<&str>, all: bool, follow: bool) -> Result<()> {
    let id = id.map(crate::queue::parse_seq_id).transpose()?;
    let path = match id {
        Some(id) => crate::runlog::failed_log_path(id)?,
        None => crate::runlog::log_path()?,
    };

    if !path.exists() {
        if let Some(id) = id {
            eprintln!("jjq: no failed check output available for item {}", id);
        } else {
            eprintln!("jjq: no run output available");
        }
        return Ok(());
    }

    // Read the initial content and its identity from one open handle: a
    // publish-by-rename between a path-based read and a separate identity
    // probe could pair the old content's offset with the new file's
    // identity, making the follower skip the replacement's content.
    let mut initial_file = File::open(&path)?;
    let initial_identity = identity_of(&initial_file);
    let mut content = String::new();
    initial_file.read_to_string(&mut content)?;
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

    // Track our read position by byte offset; use seek-based reads
    // to avoid re-reading the entire file each iteration.
    let mut offset = content.len() as u64;
    // The log is republished by rename, so a new attempt replaces the file
    // rather than appending. Byte offsets alone cannot distinguish that
    // from an append when the replacement is at least as long; track the
    // file identity too and restart from the beginning on replacement.
    let mut identity = initial_identity;

    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Identity comes from the open handle itself: reading it from the
        // path again could straddle a rename, pairing the old handle with
        // the new file's identity (and length) and skipping the start of
        // the new log.
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                eprintln!("jjq: log file disappeared");
                return Ok(());
            }
        };
        let current_identity = identity_of(&file);

        if current_identity != identity {
            // The log was replaced by a new publication; read it fresh.
            identity = current_identity;
            offset = 0;
        }

        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

        if file_len < offset {
            // File was truncated (new run started); reset to beginning
            offset = 0;
        }

        if file_len > offset {
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = String::new();
            file.read_to_string(&mut buf)?;
            for line in buf.lines() {
                if line.starts_with(crate::runlog::SENTINEL_PREFIX) {
                    return Ok(());
                }
                writeln!(out, "{}", line)?;
            }
            out.flush()?;
            offset = file_len;
        } else if !crate::lock::is_held("run")? {
            eprintln!("jjq: run process is no longer active");
            return Ok(());
        }
    }
}
