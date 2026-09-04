// ABOUTME: Queue management for jjq - sequence IDs and queue operations.
// ABOUTME: Handles bookmark-based queue state and FIFO ordering.

use anyhow::{bail, Result};
use regex::Regex;
use serde::Deserialize;
use std::fs;
use std::sync::OnceLock;

use crate::exit_codes::{self, ExitError};
use crate::jj;
use crate::meta;

/// Validate and parse a sequence ID from user input.
/// Returns the integer value on success.
pub fn parse_seq_id(input: &str) -> Result<u32> {
    if input.is_empty() {
        bail!("invalid sequence ID: empty");
    }

    // Only digits allowed
    if !input.chars().all(|c| c.is_ascii_digit()) {
        bail!("invalid sequence ID: '{}' (must be numeric)", input);
    }

    let id: u32 = input
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid sequence ID: {} (must be 1-999999)", input))?;

    if !(1..=999999).contains(&id) {
        bail!("invalid sequence ID: {} (must be 1-999999)", input);
    }

    Ok(id)
}

/// Format a sequence ID as zero-padded string for bookmark names.
pub fn format_seq_id(id: u32) -> String {
    format!("{:06}", id)
}

/// Get the next sequence ID, incrementing the counter. Allocation happens
/// inside the metadata writer, so the meta lock serializes concurrent pushes.
pub fn next_id() -> Result<u32> {
    let mut allocated = 0u32;
    meta::with_described_meta_workspace("queue: allocated sequence ID", |workspace| {
        let path = workspace.join("last_id");
        // A missing or corrupt counter means the metadata branch is damaged;
        // restarting from 0 would hand out duplicate IDs, so fail hard.
        let raw = fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "cannot read sequence counter from jjq metadata ({}); run 'jjq doctor'",
                e
            )
        })?;
        let current: u32 = raw.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "sequence counter in jjq metadata is corrupt ({:?}); run 'jjq doctor'",
                raw.trim()
            )
        })?;

        if current >= 999999 {
            return Err(
                ExitError::new(exit_codes::USAGE, "sequence ID exhausted (at 999999)").into(),
            );
        }

        allocated = current + 1;
        fs::write(&path, allocated.to_string())?;
        Ok(())
    })?;
    Ok(allocated)
}

/// Strictly parse one bookmark in a queue-state namespace. Names from other
/// jjq features, malformed IDs, and co-located bookmarks are intentionally
/// ignored rather than treated as queue entries.
fn parse_state_bookmark(name: &str) -> Option<(State, u32)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let captures = RE
        .get_or_init(|| Regex::new(r"^jjq/(queue|running|failed)/(\d{6})$").unwrap())
        .captures(name)?;
    let state = match &captures[1] {
        "queue" => State::Queued,
        "running" => State::Running,
        "failed" => State::Failed,
        _ => unreachable!("regex only matches known queue states"),
    };
    Some((state, captures[2].parse().expect("six digits fit in u32")))
}

/// One queue-state bookmark and its target as observed in a single jj
/// operation. Target fields are absent only when the bookmark is conflicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateEntry {
    pub id: u32,
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotEntry {
    name: String,
    id: u32,
    conflicted: bool,
    change_id: Option<String>,
    commit_id: Option<String>,
    description: Option<String>,
}

impl SnapshotEntry {
    fn target(&self) -> Result<StateEntry> {
        if self.conflicted {
            bail!("queue-state bookmark ({}) is conflicted", self.name);
        }
        Ok(StateEntry {
            id: self.id,
            change_id: self
                .change_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("bookmark {} has no change ID", self.name))?,
            commit_id: self
                .commit_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("bookmark {} has no commit ID", self.name))?,
            description: self.description.clone().ok_or_else(|| {
                anyhow::anyhow!("bookmark {} has no commit description", self.name)
            })?,
        })
    }
}

/// A coherent view of every queue-state namespace from one `jj bookmark
/// list` operation. Unrelated `jjq/*/NNNNNN` and co-located bookmarks are
/// ignored by strict name parsing.
pub struct StateSnapshot {
    queued: Vec<SnapshotEntry>,
    running: Vec<SnapshotEntry>,
    failed: Vec<SnapshotEntry>,
}

impl StateSnapshot {
    fn entries_mut(&mut self, state: State) -> &mut Vec<SnapshotEntry> {
        match state {
            State::Queued => &mut self.queued,
            State::Running => &mut self.running,
            State::Failed => &mut self.failed,
        }
    }

    pub fn queued(&self) -> Result<Vec<StateEntry>> {
        self.queued.iter().map(SnapshotEntry::target).collect()
    }

    pub fn running(&self) -> Result<Vec<StateEntry>> {
        self.running.iter().map(SnapshotEntry::target).collect()
    }

    pub fn failed(&self) -> Result<Vec<StateEntry>> {
        self.failed.iter().map(SnapshotEntry::target).collect()
    }

    pub fn entry(&self, state: State, id: u32) -> Result<Option<StateEntry>> {
        let entries = match state {
            State::Queued => &self.queued,
            State::Running => &self.running,
            State::Failed => &self.failed,
        };
        entries
            .iter()
            .find(|entry| entry.id == id)
            .map(SnapshotEntry::target)
            .transpose()
    }

    /// Look up one ID using the command-facing precedence: queued, failed,
    /// then running. Duplicate states can exist after an interrupted
    /// create-before-delete transition, so callers that need every state use
    /// the collection accessors instead.
    pub fn state_for(&self, id: u32) -> Result<Option<(State, StateEntry)>> {
        for state in [State::Queued, State::Failed, State::Running] {
            if let Some(entry) = self.entry(state, id)? {
                return Ok(Some((state, entry)));
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Queued,
    Running,
    Failed,
}

impl State {
    fn namespace(self) -> &'static str {
        match self {
            Self::Queued => "queue",
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }
}

#[derive(Deserialize)]
struct RawSnapshotEntry {
    name: String,
    conflicted: bool,
    change_id: Option<String>,
    commit_id: Option<String>,
    description: Option<String>,
}

const SNAPSHOT_TEMPLATE: &str = r#""{\"name\":" ++ name.escape_json() ++ ",\"conflicted\":" ++ conflict ++ ",\"change_id\":" ++ if(conflict, "null", stringify(normal_target.change_id().short()).escape_json()) ++ ",\"commit_id\":" ++ if(conflict, "null", stringify(normal_target.commit_id()).escape_json()) ++ ",\"description\":" ++ if(conflict, "null", normal_target.description().escape_json()) ++ "}\n""#;

/// Read queue, running, and failed bookmarks and their targets in one
/// repository snapshot.
pub fn state_snapshot() -> Result<StateSnapshot> {
    let output = jj::run_ok(&[
        "bookmark",
        "list",
        "-r",
        r#"bookmarks(glob:"jjq/*/??????")"#,
        "-T",
        SNAPSHOT_TEMPLATE,
    ])?;
    parse_state_snapshot(&output)
}

fn parse_state_snapshot(output: &str) -> Result<StateSnapshot> {
    let mut snapshot = StateSnapshot {
        queued: Vec::new(),
        running: Vec::new(),
        failed: Vec::new(),
    };
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let raw: RawSnapshotEntry = serde_json::from_str(line)
            .map_err(|error| anyhow::anyhow!("invalid jj bookmark snapshot: {}", error))?;
        let Some((state, id)) = parse_state_bookmark(&raw.name) else {
            continue;
        };
        let entries = snapshot.entries_mut(state);
        entries.push(SnapshotEntry {
            name: raw.name,
            id,
            conflicted: raw.conflicted,
            change_id: raw.change_id,
            commit_id: raw.commit_id,
            description: raw.description,
        });
    }
    snapshot.queued.sort_by_key(|entry| entry.id);
    snapshot.running.sort_by_key(|entry| entry.id);
    snapshot.failed.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(snapshot)
}

fn state_ids(state: State) -> Result<Vec<u32>> {
    let bookmarks = jj::bookmark_list_glob(&format!("jjq/{}/??????", state.namespace()))?;
    let mut ids = bookmarks
        .iter()
        .filter_map(|bookmark| parse_state_bookmark(bookmark))
        .filter_map(|(parsed_state, id)| (parsed_state == state).then_some(id))
        .collect::<Vec<_>>();
    ids.sort();
    Ok(ids)
}

/// Get all queued items sorted by sequence ID (ascending).
pub fn get_queue() -> Result<Vec<u32>> {
    state_ids(State::Queued)
}

/// Get all running (claimed) items sorted by sequence ID (ascending). While
/// a runner is active these are the items it is processing; with the run
/// lock free they are orphans from a crashed runner.
pub fn get_running() -> Result<Vec<u32>> {
    state_ids(State::Running)
}

/// Get the next item to process (lowest sequence ID).
pub fn next_item() -> Result<Option<u32>> {
    let queue = get_queue()?;
    Ok(queue.into_iter().next())
}

/// Get the queue bookmark name for an ID.
pub fn queue_bookmark(id: u32) -> String {
    state_bookmark(State::Queued, id)
}

/// Get the failed bookmark name for an ID.
pub fn failed_bookmark(id: u32) -> String {
    state_bookmark(State::Failed, id)
}

/// Get the running (claim) bookmark name for an ID.
pub fn running_bookmark(id: u32) -> String {
    state_bookmark(State::Running, id)
}

fn state_bookmark(state: State, id: u32) -> String {
    format!("jjq/{}/{}", state.namespace(), format_seq_id(id))
}

/// Check if a queue item exists.
pub fn queue_item_exists(id: u32) -> Result<bool> {
    jj::bookmark_exists(&queue_bookmark(id))
}

/// Check if a failed item exists.
pub fn failed_item_exists(id: u32) -> Result<bool> {
    jj::bookmark_exists(&failed_bookmark(id))
}

/// Check if a running (claimed) item exists.
pub fn running_item_exists(id: u32) -> Result<bool> {
    jj::bookmark_exists(&running_bookmark(id))
}

#[cfg(test)]
mod tests {
    use super::{parse_state_bookmark, parse_state_snapshot, State};

    #[test]
    fn parses_only_strict_queue_state_bookmark_names() {
        assert_eq!(
            parse_state_bookmark("jjq/queue/000001"),
            Some((State::Queued, 1))
        );
        assert_eq!(
            parse_state_bookmark("jjq/running/999999"),
            Some((State::Running, 999_999))
        );
        assert_eq!(parse_state_bookmark("jjq/failed/000001/child"), None);
        assert_eq!(parse_state_bookmark("jjq/queue/00001"), None);
        assert_eq!(parse_state_bookmark("jjq/review/000001"), None);
    }

    #[test]
    fn parses_sorts_and_ignores_unrelated_snapshot_entries() {
        let snapshot = parse_state_snapshot(
            r#"{"name":"jjq/queue/000002","conflicted":false,"change_id":"q2","commit_id":"c2","description":"second\nline"}
{"name":"jjq/review/000009","conflicted":false,"change_id":"review","commit_id":"cr","description":"ignored"}
{"name":"jjq/failed/000003","conflicted":false,"change_id":"f3","commit_id":"c3","description":"failed"}
{"name":"jjq/queue/000001","conflicted":false,"change_id":"q1","commit_id":"c1","description":"first"}
{"name":"main","conflicted":false,"change_id":"q1","commit_id":"c1","description":"co-located"}
"#,
        )
        .unwrap();

        assert_eq!(
            snapshot
                .queued()
                .unwrap()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(snapshot.failed().unwrap()[0].id, 3);
        let (state, entry) = snapshot.state_for(2).unwrap().unwrap();
        assert_eq!(state, State::Queued);
        assert_eq!(entry.description, "second\nline");
    }

    #[test]
    fn reports_conflicts_only_when_the_state_is_consumed() {
        let snapshot = parse_state_snapshot(
            r#"{"name":"jjq/queue/000001","conflicted":false,"change_id":"q1","commit_id":"c1","description":"queued"}
{"name":"jjq/failed/000002","conflicted":true,"change_id":null,"commit_id":null,"description":null}
"#,
        )
        .unwrap();

        assert_eq!(snapshot.queued().unwrap()[0].id, 1);
        assert!(snapshot
            .failed()
            .unwrap_err()
            .to_string()
            .contains("jjq/failed/000002"));
    }
}
