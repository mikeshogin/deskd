//! Unified inbox — JSONL-based message storage for all incoming messages.
//!
//! Stores messages at `~/.deskd/inboxes/{inbox_name}.jsonl`.
//! Each line is a JSON-serialized `InboxMessage`.
//!
//! This is separate from the task-result inbox at `~/.deskd/inbox/` — the
//! unified inbox captures ALL messages (not just task results) for later
//! retrieval via MCP tools.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use tracing::warn;

/// Maximum messages to keep per inbox file during rotation.
const MAX_MESSAGES_PER_INBOX: usize = 10_000;

/// A single message in the unified inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Timestamp of when the message was received.
    pub ts: DateTime<Utc>,
    /// Source identifier (e.g. "telegram", "github_poll", "schedule", "agent").
    pub source: String,
    /// Optional sender username or ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Message text content.
    pub text: String,
    /// Arbitrary metadata (e.g. chat_id, repo, issue number).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

/// Base directory for unified inboxes: `~/.deskd/inboxes/`.
fn inboxes_base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".deskd").join("inboxes")
}

/// Full path for an inbox file given a base directory.
fn inbox_path_in(base: &Path, inbox_name: &str) -> PathBuf {
    base.join(format!("{}.jsonl", inbox_name))
}

/// Append a message to the specified inbox.
///
/// Creates parent directories as needed. Performs rotation when the file
/// exceeds `MAX_MESSAGES_PER_INBOX` lines.
pub fn write_message(inbox_name: &str, message: &InboxMessage) -> Result<()> {
    write_message_to(&inboxes_base_dir(), inbox_name, message)
}

fn write_message_to(base: &Path, inbox_name: &str, message: &InboxMessage) -> Result<()> {
    let path = inbox_path_in(base, inbox_name);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create inbox dir: {}", parent.display()))?;
    }

    let line = serde_json::to_string(message).context("failed to serialize inbox message")?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open inbox: {}", path.display()))?;

    writeln!(file, "{}", line)
        .with_context(|| format!("failed to write to inbox: {}", path.display()))?;

    // Check if rotation is needed (count lines cheaply).
    drop(file);
    if let Ok(count) = count_lines(&path)
        && count > MAX_MESSAGES_PER_INBOX
        && let Err(e) = rotate(&path, MAX_MESSAGES_PER_INBOX)
    {
        warn!(inbox = %inbox_name, error = %e, "inbox rotation failed");
    }

    Ok(())
}

/// Read messages from the specified inbox.
///
/// Returns up to `limit` messages, optionally filtered to those after `since`.
/// Messages are returned in chronological order (oldest first).
pub fn read_messages(
    inbox_name: &str,
    limit: usize,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<InboxMessage>> {
    read_messages_from(&inboxes_base_dir(), inbox_name, limit, since)
}

fn read_messages_from(
    base: &Path,
    inbox_name: &str,
    limit: usize,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<InboxMessage>> {
    let path = inbox_path_in(base, inbox_name);
    if !path.exists() {
        return Ok(vec![]);
    }

    let file = std::fs::File::open(&path)
        .with_context(|| format!("failed to open inbox: {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut messages = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<InboxMessage>(&line) {
            Ok(msg) => {
                if let Some(ref since_ts) = since
                    && msg.ts <= *since_ts
                {
                    continue;
                }
                messages.push(msg);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "skipping malformed inbox line");
            }
        }
    }

    // Return the last `limit` messages (most recent).
    if messages.len() > limit {
        messages = messages.split_off(messages.len() - limit);
    }

    Ok(messages)
}

/// Search messages across one or all inboxes.
///
/// If `inbox_name` is `Some`, searches only that inbox. Otherwise searches all.
/// Returns up to `limit` matching messages (case-insensitive substring match).
pub fn search_messages(
    inbox_name: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<InboxMessage>> {
    search_messages_in(&inboxes_base_dir(), inbox_name, query, limit)
}

fn search_messages_in(
    base: &Path,
    inbox_name: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<InboxMessage>> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    let inboxes = if let Some(name) = inbox_name {
        vec![name.to_string()]
    } else {
        list_inbox_names_in(base)?
    };

    'outer: for name in &inboxes {
        let path = inbox_path_in(base, name);
        if !path.exists() {
            continue;
        }
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<InboxMessage>(&line)
                && (msg.text.to_lowercase().contains(&query_lower)
                    || msg.source.to_lowercase().contains(&query_lower)
                    || msg
                        .from
                        .as_ref()
                        .is_some_and(|f| f.to_lowercase().contains(&query_lower)))
            {
                results.push(msg);
                if results.len() >= limit {
                    break 'outer;
                }
            }
        }
    }

    Ok(results)
}

/// List all available inboxes with message counts.
///
/// Returns a list of `(inbox_name, message_count)` tuples.
pub fn list_inboxes() -> Result<Vec<(String, usize)>> {
    list_inboxes_in(&inboxes_base_dir())
}

fn list_inboxes_in(base: &Path) -> Result<Vec<(String, usize)>> {
    let mut inboxes = Vec::new();
    let names = list_inbox_names_in(base)?;
    for name in names {
        let path = inbox_path_in(base, &name);
        let count = count_lines(&path).unwrap_or(0);
        inboxes.push((name, count));
    }
    inboxes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(inboxes)
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/// Recursively find all `.jsonl` files under a base dir and return
/// their names relative to the base (without the `.jsonl` extension).
fn list_inbox_names_in(base: &Path) -> Result<Vec<String>> {
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    collect_jsonl_files(base, base, &mut names)?;
    Ok(names)
}

fn collect_jsonl_files(dir: &Path, base: &Path, names: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, base, names)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && let Ok(rel) = path.strip_prefix(base)
        {
            let name = rel.with_extension("").to_string_lossy().replace('\\', "/");
            names.push(name);
        }
    }
    Ok(())
}

/// Count lines in a file.
fn count_lines(path: &Path) -> Result<usize> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    Ok(reader.lines().count())
}

/// Rotate an inbox file: keep only the last `keep` lines.
fn rotate(path: &Path, keep: usize) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= keep {
        return Ok(());
    }
    let truncated = &lines[lines.len() - keep..];
    let new_content = truncated.join("\n") + "\n";
    std::fs::write(path, new_content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    /// Create a unique temporary base directory for each test.
    fn test_base() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deskd_unified_inbox_test_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_msg(text: &str, source: &str) -> InboxMessage {
        InboxMessage {
            ts: Utc::now(),
            source: source.to_string(),
            from: Some("testuser".to_string()),
            text: text.to_string(),
            metadata: json!({}),
        }
    }

    #[test]
    fn test_write_and_read() {
        let base = test_base();

        let msg1 = make_msg("hello world", "telegram");
        let msg2 = make_msg("second message", "telegram");

        write_message_to(&base, "test/chat1", &msg1).unwrap();
        write_message_to(&base, "test/chat1", &msg2).unwrap();

        let msgs = read_messages_from(&base, "test/chat1", 100, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "hello world");
        assert_eq!(msgs[1].text, "second message");
    }

    #[test]
    fn test_read_with_limit() {
        let base = test_base();

        for i in 0..10 {
            let msg = make_msg(&format!("msg {}", i), "test");
            write_message_to(&base, "test/limited", &msg).unwrap();
        }

        let msgs = read_messages_from(&base, "test/limited", 3, None).unwrap();
        assert_eq!(msgs.len(), 3);
        // Should be the last 3 messages
        assert_eq!(msgs[0].text, "msg 7");
        assert_eq!(msgs[1].text, "msg 8");
        assert_eq!(msgs[2].text, "msg 9");
    }

    #[test]
    fn test_read_with_since() {
        let base = test_base();

        let old_msg = InboxMessage {
            ts: DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            source: "test".to_string(),
            from: None,
            text: "old message".to_string(),
            metadata: serde_json::Value::Null,
        };
        let new_msg = InboxMessage {
            ts: DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            source: "test".to_string(),
            from: None,
            text: "new message".to_string(),
            metadata: serde_json::Value::Null,
        };

        write_message_to(&base, "test/since", &old_msg).unwrap();
        write_message_to(&base, "test/since", &new_msg).unwrap();

        let since = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msgs = read_messages_from(&base, "test/since", 100, Some(since)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "new message");
    }

    #[test]
    fn test_read_nonexistent_inbox() {
        let base = test_base();
        let msgs = read_messages_from(&base, "nonexistent/inbox", 100, None).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_search_messages() {
        let base = test_base();

        write_message_to(&base, "test/search1", &make_msg("alpha beta", "src1")).unwrap();
        write_message_to(&base, "test/search1", &make_msg("gamma delta", "src1")).unwrap();
        write_message_to(&base, "test/search2", &make_msg("alpha omega", "src2")).unwrap();

        // Search across all inboxes
        let results = search_messages_in(&base, None, "alpha", 100).unwrap();
        assert_eq!(results.len(), 2);

        // Search specific inbox
        let results = search_messages_in(&base, Some("test/search1"), "gamma", 100).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "gamma delta");

        // Search with limit
        let results = search_messages_in(&base, None, "alpha", 1).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_case_insensitive() {
        let base = test_base();

        write_message_to(&base, "test/case", &make_msg("Hello World", "src")).unwrap();

        let results = search_messages_in(&base, Some("test/case"), "hello", 100).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_list_inboxes() {
        let base = test_base();

        write_message_to(&base, "telegram/123", &make_msg("msg1", "tg")).unwrap();
        write_message_to(&base, "telegram/123", &make_msg("msg2", "tg")).unwrap();
        write_message_to(&base, "github/owner-repo", &make_msg("issue", "gh")).unwrap();

        let inboxes = list_inboxes_in(&base).unwrap();
        assert_eq!(inboxes.len(), 2);

        let tg = inboxes.iter().find(|(n, _)| n == "telegram/123").unwrap();
        assert_eq!(tg.1, 2);

        let gh = inboxes
            .iter()
            .find(|(n, _)| n == "github/owner-repo")
            .unwrap();
        assert_eq!(gh.1, 1);
    }

    #[test]
    fn test_rotation() {
        let base = test_base();

        let path = inbox_path_in(&base, "test/rotate");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let mut file = std::fs::File::create(&path).unwrap();
        for i in 0..20 {
            let msg = make_msg(&format!("line {}", i), "test");
            let line = serde_json::to_string(&msg).unwrap();
            writeln!(file, "{}", line).unwrap();
        }
        drop(file);

        assert_eq!(count_lines(&path).unwrap(), 20);

        rotate(&path, 10).unwrap();

        assert_eq!(count_lines(&path).unwrap(), 10);

        // Verify the kept messages are the last 10
        let msgs = read_messages_from(&base, "test/rotate", 100, None).unwrap();
        assert_eq!(msgs.len(), 10);
        assert_eq!(msgs[0].text, "line 10");
        assert_eq!(msgs[9].text, "line 19");
    }

    #[test]
    fn test_search_by_from_field() {
        let base = test_base();

        let msg = InboxMessage {
            ts: Utc::now(),
            source: "telegram".to_string(),
            from: Some("johndoe".to_string()),
            text: "some text".to_string(),
            metadata: serde_json::Value::Null,
        };
        write_message_to(&base, "test/from", &msg).unwrap();

        let results = search_messages_in(&base, Some("test/from"), "johndoe", 100).unwrap();
        assert_eq!(results.len(), 1);
    }
}
