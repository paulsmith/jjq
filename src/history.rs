// ABOUTME: Durable, append-only lifecycle history stored on jjq's metadata branch.
// ABOUTME: Events are audit records; queue bookmarks remain authoritative state.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;
use crate::meta;
use crate::queue;

pub const SCHEMA_VERSION: u32 = 1;

/// Stable lifecycle event names. Keep these strings centralized: they are a
/// public part of `jjq history --json`, unlike the human-facing text view.
pub mod kind {
    pub const QUEUED: &str = "queued";
    pub const CLAIMED: &str = "claimed";
    pub const RECOVERED: &str = "recovered";
    pub const CHECK_STARTED: &str = "check_started";
    pub const FAILED: &str = "failed";
    pub const SKIPPED_EMPTY: &str = "skipped_empty";
    pub const LANDED: &str = "landed";
    pub const REQUEUED: &str = "requeued";
    pub const DELETED: &str = "deleted";
    pub const REPLACED: &str = "replaced";
    pub const CANCEL_REQUESTED: &str = "cancel_requested";
    pub const CANCELED: &str = "canceled";
}

/// A lifecycle fact captured after jjq changes a queue bookmark.
///
/// Fields intentionally use stable, explicit names. Facts which are not
/// known for a transition are left out of the on-disk JSON rather than being
/// represented by ambiguous empty strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub version: u32,
    pub timestamp: String,
    pub sequence_id: u32,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_commit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trunk_commit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_commit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_sequence_id: Option<u32>,
}

impl Event {
    pub fn new(sequence_id: u32, kind: &'static str) -> Self {
        Self {
            version: SCHEMA_VERSION,
            timestamp: utc_now(),
            sequence_id,
            kind: kind.to_string(),
            candidate_change_id: None,
            candidate_commit_id: None,
            trunk_commit_id: None,
            result_change_id: None,
            result_commit_id: None,
            strategy: None,
            failure_reason: None,
            conflict_paths: None,
            elapsed_ms: None,
            related_sequence_id: None,
        }
    }
}

/// Persist one or more events in one metadata transaction. Event ordinals are
/// allocated while holding the metadata lock, so concurrent writers cannot
/// choose the same file name or discard one another's history.
pub fn record_many(events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let description = format!("history: recorded {} event(s)", events.len());
    meta::with_described_meta_workspace(&description, |workspace| write_events(workspace, events))
}

pub fn record(event: Event) -> Result<()> {
    record_many(&[event])
}

/// Atomically persist a claimed event and the deterministic recovery metadata
/// for the workspace that will materialize that claim. The caller can publish
/// this under publication.lock before `jj workspace add`: a crash then leaves
/// enough durable information for clean/recovery without a second metadata
/// transaction or a window where the claim has history but no workspace path.
pub fn record_claim_with_workspace(
    event: Event,
    workspace_path: &str,
    workspace_name: &str,
) -> Result<()> {
    if event.kind != kind::CLAIMED {
        bail!("workspace metadata can only be attached to a claimed event");
    }
    let description =
        claim_workspace_description(event.sequence_id, workspace_path, workspace_name);
    meta::with_described_meta_workspace(&description, |workspace| {
        write_events(workspace, std::slice::from_ref(&event))?;
        let dir = workspace.join("workspace");
        fs::create_dir_all(&dir)?;
        let id = queue::format_seq_id(event.sequence_id);
        fs::write(dir.join(&id), workspace_path)?;
        fs::write(dir.join(format!("{}.name", id)), workspace_name)?;
        Ok(())
    })
}

fn claim_workspace_description(id: u32, workspace_path: &str, workspace_name: &str) -> String {
    format!(
        "history: recorded 1 event(s)\nSequence-Id: {}\nWorkspace: {}\nWorkspace-Name: {}",
        id, workspace_path, workspace_name
    )
}

fn write_events(workspace: &Path, events: &[Event]) -> Result<()> {
    for event in events {
        let dir = workspace
            .join("history")
            .join(queue::format_seq_id(event.sequence_id));
        fs::create_dir_all(&dir)?;
        let ordinal = next_ordinal(&dir)?;
        let json = serde_json::to_vec_pretty(event)?;
        fs::write(dir.join(format!("{:06}.json", ordinal)), json)?;
    }
    Ok(())
}

/// Read all events for a submission using exactly one pinned metadata
/// snapshot. Missing history is a normal result for repositories created
/// before this feature.
pub fn read(id: u32) -> Result<Vec<Event>> {
    let Some(snapshot) = config::snapshot()? else {
        return Ok(vec![]);
    };
    let prefix = format!("history/{}", queue::format_seq_id(id));
    let mut files = snapshot.files_contents(&prefix)?;
    files.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut events = Vec::with_capacity(files.len());
    for (path, contents) in files {
        if !path.ends_with(".json") {
            continue;
        }
        events.push(decode_event(&path, &contents, Some(&path))?);
    }
    Ok(events)
}

/// List all submission IDs represented by history in one pinned snapshot.
pub fn read_by_change(change_id: &str) -> Result<Vec<(u32, Vec<Event>)>> {
    let Some(snapshot) = config::snapshot()? else {
        return Ok(vec![]);
    };
    let mut files = snapshot.files_contents("history")?;
    files.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut submissions: BTreeMap<u32, Vec<Event>> = BTreeMap::new();
    for (path, contents) in files {
        let Some(id_part) = path
            .strip_prefix("history/")
            .and_then(|p| p.split('/').next())
        else {
            continue;
        };
        let Ok(id) = id_part.parse::<u32>() else {
            continue;
        };
        // The old per-submission reader reconstructed this canonical prefix
        // from the parsed ID, so non-canonical lookalike directories did not
        // contribute events. Preserve that behavior in the bulk scan.
        if id_part != queue::format_seq_id(id) {
            continue;
        }
        if !path.ends_with(".json") {
            continue;
        }
        submissions
            .entry(id)
            .or_default()
            .push(decode_event(&path, &contents, None)?);
    }
    let submissions: Vec<_> = submissions
        .into_iter()
        .filter(|(_, events)| {
            events
                .iter()
                .any(|event| event.candidate_change_id.as_deref() == Some(change_id))
        })
        .collect();
    Ok(submissions)
}

fn decode_event(path: &str, contents: &str, version_path: Option<&str>) -> Result<Event> {
    let event: Event = serde_json::from_str(contents)
        .with_context(|| format!("invalid jjq history event {}", path))?;
    if event.version != SCHEMA_VERSION {
        if let Some(version_path) = version_path {
            bail!(
                "unsupported jjq history event version {} in {}",
                event.version,
                version_path
            );
        }
        bail!("unsupported jjq history event version {}", event.version);
    }
    Ok(event)
}

fn next_ordinal(dir: &std::path::Path) -> Result<u32> {
    let mut highest = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(value) = stem.parse::<u32>() {
            highest = highest.max(value);
        }
    }
    highest
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("history event ordinal exhausted"))
}

// Civil-from-days, adapted from Howard Hinnant's public-domain algorithm.
// Avoiding a clock dependency keeps the binary's dependency surface small.
fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        time / 3_600,
        (time % 3_600) / 60,
        time % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + if month <= 2 { 1 } else { 0 }, month, day)
}

#[cfg(test)]
mod tests {
    use super::{Event, claim_workspace_description, decode_event};

    #[test]
    fn combined_claim_description_preserves_operation_log_fields() {
        assert_eq!(
            claim_workspace_description(7, "/tmp/jjq.abc", "jjq-run-000007-token"),
            "history: recorded 1 event(s)\nSequence-Id: 7\nWorkspace: /tmp/jjq.abc\nWorkspace-Name: jjq-run-000007-token"
        );
    }

    #[test]
    fn decode_event_rejects_unknown_schema_versions_with_its_path() {
        let mut event = Event::new(7, "queued");
        event.version = 2;
        let error = decode_event(
            "history/000007/000001.json",
            &serde_json::to_string(&event).unwrap(),
            Some("history/000007/000001.json"),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unsupported jjq history event version 2 in history/000007/000001.json"
        );

        let error = decode_event(
            "history/000007/000001.json",
            &serde_json::to_string(&event).unwrap(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unsupported jjq history event version 2"
        );
    }
}
