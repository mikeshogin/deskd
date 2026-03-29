use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Maximum number of entries to keep in the JSONL file.
const MAX_ENTRIES: usize = 10_000;

/// A single task log entry, written as one JSON line per completed task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLog {
    /// When the task started (UTC, ISO 8601).
    pub ts: String,
    /// Channel: telegram, github_poll, schedule, shell, reminder, agent:<name>, cli.
    pub source: String,
    /// Number of Claude tool-use loops.
    pub turns: u32,
    /// Cost in USD for this task.
    pub cost: f64,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
    /// ok, error, skip, empty.
    pub status: String,
    /// First 60 chars of task text.
    pub task: String,
    /// Error message if status is "error".
    pub error: Option<String>,
    /// Message ID from the bus envelope.
    pub msg_id: String,
}

/// Return the path to the task log file for a given agent.
/// Convention: ~/.deskd/logs/{agent}/tasks.jsonl
pub fn log_path(agent_name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home)
        .join(".deskd")
        .join("logs")
        .join(agent_name);
    std::fs::create_dir_all(&dir).ok();
    dir.join("tasks.jsonl")
}

/// Append a task log entry to the agent's JSONL file.
/// Performs log rotation if the file exceeds MAX_ENTRIES.
pub fn log_task(agent_name: &str, entry: &TaskLog) -> Result<()> {
    let path = log_path(agent_name);
    log_task_to_path(&path, entry)
}

/// Append a task log entry to a specific file path.
pub fn log_task_to_path(path: &PathBuf, entry: &TaskLog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut line = serde_json::to_string(entry).context("failed to serialize task log entry")?;
    line.push('\n');

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open task log: {}", path.display()))?;

    file.write_all(line.as_bytes())
        .with_context(|| format!("failed to write task log: {}", path.display()))?;

    // Check if rotation is needed (count lines).
    rotate_if_needed(path)?;

    Ok(())
}

/// Keep only the last MAX_ENTRIES lines in the file.
fn rotate_if_needed(path: &PathBuf) -> Result<()> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let reader = std::io::BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<std::io::Result<Vec<_>>>()?;

    if lines.len() > MAX_ENTRIES {
        let keep = &lines[lines.len() - MAX_ENTRIES..];
        let mut file = std::fs::File::create(path)?;
        for line in keep {
            writeln!(file, "{}", line)?;
        }
    }

    Ok(())
}

/// Read task log entries for an agent, applying optional filters.
pub fn read_logs(
    agent_name: &str,
    limit: usize,
    source_filter: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<TaskLog>> {
    let path = log_path(agent_name);
    read_logs_from_path(&path, limit, source_filter, since)
}

/// Read task log entries from a specific file path, applying optional filters.
pub fn read_logs_from_path(
    path: &PathBuf,
    limit: usize,
    source_filter: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<TaskLog>> {
    if !path.exists() {
        return Ok(vec![]);
    }

    let file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut entries: Vec<TaskLog> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: TaskLog = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue, // skip malformed lines
        };

        // Apply source filter.
        if let Some(src) = source_filter
            && entry.source != src
        {
            continue;
        }

        // Apply time filter.
        if let Some(ref cutoff) = since
            && let Ok(ts) = DateTime::parse_from_rfc3339(&entry.ts)
            && ts < *cutoff
        {
            continue;
        }

        entries.push(entry);
    }

    // Return last `limit` entries (newest last in file, so take from end).
    if entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }

    Ok(entries)
}

/// Format duration_ms as human-readable (e.g. "45s", "2m10s").
pub fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    if total_secs == 0 {
        return "0s".to_string();
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins > 0 {
        format!("{}m{}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Print a formatted table of task log entries.
pub fn print_table(entries: &[TaskLog]) {
    println!(
        "{:<20} {:<14} {:>5}  {:>7}  {:>9}  {:<6} TASK",
        "TIMESTAMP", "SOURCE", "TURNS", "COST", "DURATION", "STATUS"
    );
    for e in entries {
        // Format timestamp: show just date+time, trim sub-seconds.
        let ts_display = if e.ts.len() >= 19 { &e.ts[..19] } else { &e.ts };
        let ts_display = ts_display.replace('T', " ");

        println!(
            "{:<20} {:<14} {:>5}  ${:>6.2}  {:>9}  {:<6} {}",
            ts_display,
            e.source,
            e.turns,
            e.cost,
            format_duration(e.duration_ms),
            e.status,
            e.task,
        );
    }
}

/// Print raw JSONL output.
pub fn print_json(entries: &[TaskLog]) {
    for e in entries {
        if let Ok(line) = serde_json::to_string(e) {
            println!("{}", line);
        }
    }
}

/// Print cost summary.
pub fn print_cost_summary(agent_name: &str, entries: &[TaskLog], since_label: Option<&str>) {
    use std::collections::HashMap;

    println!("Agent: {}", agent_name);
    if let Some(label) = since_label {
        println!("Period: last {}", label);
    }
    println!();

    let total_tasks = entries.len();
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum();
    let total_turns: u32 = entries.iter().map(|e| e.turns).sum();

    println!(
        "Total: {} tasks, ${:.2}, {} turns",
        total_tasks, total_cost, total_turns
    );
    println!();

    // Group by source.
    let mut by_source: HashMap<&str, (usize, f64)> = HashMap::new();
    for e in entries {
        let entry = by_source.entry(&e.source).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += e.cost;
    }

    if by_source.is_empty() {
        return;
    }

    // Sort by cost descending.
    let mut sources: Vec<_> = by_source.into_iter().collect();
    sources.sort_by(|a, b| {
        b.1.1
            .partial_cmp(&a.1.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("By source:");
    for (source, (count, cost)) in &sources {
        let pct = if total_cost > 0.0 {
            cost / total_cost * 100.0
        } else {
            0.0
        };
        println!(
            "  {:<14} {:>3} tasks  ${:.2}  ({:.0}%)",
            source, count, cost, pct
        );
    }
}

/// Truncate a string to at most `max` characters, respecting char boundaries.
pub fn truncate_task(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        PathBuf::from(format!(
            "/tmp/deskd-test-tasklog-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ))
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(500), "0s");
        assert_eq!(format_duration(1000), "1s");
        assert_eq!(format_duration(45000), "45s");
        assert_eq!(format_duration(130_000), "2m10s");
        assert_eq!(format_duration(60_000), "1m0s");
    }

    #[test]
    fn test_truncate_task() {
        assert_eq!(truncate_task("short", 60), "short");
        let long = "a".repeat(100);
        let result = truncate_task(&long, 60);
        assert!(result.len() <= 63); // 60 + "..."
        assert!(result.ends_with("..."));
    }

    fn test_entry(source: &str, task: &str, ts: &str) -> TaskLog {
        TaskLog {
            ts: ts.to_string(),
            source: source.to_string(),
            turns: 12,
            cost: 0.42,
            duration_ms: 45000,
            status: "ok".to_string(),
            task: task.to_string(),
            error: None,
            msg_id: "test-uuid".to_string(),
        }
    }

    #[test]
    fn test_log_and_read() {
        let tmp = temp_dir();
        let path = tmp.join("tasks.jsonl");

        let entry = test_entry("telegram", "Test task", "2026-03-28T14:23:01Z");
        log_task_to_path(&path, &entry).unwrap();

        let entries = read_logs_from_path(&path, 20, None, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "telegram");
        assert_eq!(entries[0].turns, 12);
        assert_eq!(entries[0].cost, 0.42);

        // Test source filter.
        let filtered = read_logs_from_path(&path, 20, Some("github_poll"), None).unwrap();
        assert_eq!(filtered.len(), 0);

        let filtered = read_logs_from_path(&path, 20, Some("telegram"), None).unwrap();
        assert_eq!(filtered.len(), 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_log_rotation() {
        let tmp = temp_dir();
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("tasks.jsonl");

        // Write MAX_ENTRIES + 100 lines directly.
        {
            let mut file = std::fs::File::create(&path).unwrap();
            for i in 0..(MAX_ENTRIES + 100) {
                let entry = TaskLog {
                    ts: format!("2026-03-28T14:00:{:02}Z", i % 60),
                    source: "test".to_string(),
                    turns: 1,
                    cost: 0.01,
                    duration_ms: 1000,
                    status: "ok".to_string(),
                    task: format!("task {}", i),
                    error: None,
                    msg_id: format!("id-{}", i),
                };
                let line = serde_json::to_string(&entry).unwrap();
                writeln!(file, "{}", line).unwrap();
            }
        }

        rotate_if_needed(&path).unwrap();

        // Count lines after rotation.
        let file = std::fs::File::open(&path).unwrap();
        let count = std::io::BufReader::new(file).lines().count();
        assert_eq!(count, MAX_ENTRIES);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_since_filter() {
        let tmp = temp_dir();
        let path = tmp.join("tasks.jsonl");

        let old = test_entry("telegram", "old task", "2026-03-27T10:00:00Z");
        let recent = test_entry("telegram", "recent task", "2026-03-28T14:00:00Z");

        log_task_to_path(&path, &old).unwrap();
        log_task_to_path(&path, &recent).unwrap();

        let cutoff = DateTime::parse_from_rfc3339("2026-03-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let entries = read_logs_from_path(&path, 20, None, Some(cutoff)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task, "recent task");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_cost_summary_does_not_panic() {
        let entries = vec![
            test_entry("telegram", "task 1", "2026-03-28T14:00:00Z"),
            test_entry("github_poll", "task 2", "2026-03-28T14:05:00Z"),
        ];
        // Just verify it doesn't panic.
        print_cost_summary("test", &entries, Some("24h"));
    }

    #[test]
    fn test_empty_log_file() {
        let tmp = temp_dir();
        let path = tmp.join("tasks.jsonl");

        let entries = read_logs_from_path(&path, 20, None, None).unwrap();
        assert!(entries.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_log_path() {
        // Just verify the path structure contains the expected components.
        let path = log_path("myagent");
        assert!(
            path.to_string_lossy()
                .contains(".deskd/logs/myagent/tasks.jsonl")
        );
    }
}
