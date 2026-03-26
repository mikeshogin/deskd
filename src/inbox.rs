//! File-based inbox for async agent communication.
//!
//! Workers write task results to ~/.deskd/inbox/<agent>/<timestamp>_<id>.json.
//! `deskd agent read <name>` reads and optionally clears the inbox.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single inbox entry (task result or error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub id: String,
    pub agent: String,
    pub source: String,
    pub task: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub in_reply_to: String,
    pub timestamp: String,
}

/// Get the inbox directory for a given agent.
fn inbox_dir(agent: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("inbox").join(agent);
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Write a task result to the inbox.
pub fn write(agent: &str, entry: &InboxEntry) -> Result<()> {
    let dir = inbox_dir(agent);
    let filename = format!(
        "{}_{}.json",
        chrono::Utc::now().format("%Y%m%d_%H%M%S"),
        &entry.id[..8.min(entry.id.len())]
    );
    let path = dir.join(&filename);
    let json = serde_json::to_string_pretty(entry)?;
    std::fs::write(&path, json).with_context(|| format!("failed to write inbox entry: {}", path.display()))?;
    tracing::info!(agent = %agent, path = %path.display(), "wrote inbox entry");
    Ok(())
}

/// Read all inbox entries for an agent, sorted by filename (oldest first).
pub fn read(agent: &str) -> Result<Vec<(PathBuf, InboxEntry)>> {
    let dir = inbox_dir(agent);
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut entries: Vec<(PathBuf, InboxEntry)> = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();

    paths.sort();

    for path in paths {
        let content = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<InboxEntry>(&content) {
            Ok(entry) => entries.push((path, entry)),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping malformed inbox entry");
            }
        }
    }

    Ok(entries)
}

/// Remove specific inbox files (after reading).
pub fn clear(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        std::fs::remove_file(path).ok();
    }
    Ok(())
}
