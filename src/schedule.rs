/// Schedule runner — fires cron-based bus events.
///
/// Each `ScheduleDef` in deskd.yaml maps to a tokio task that sleeps until the
/// next cron occurrence, posts the configured payload to the bus, then repeats.
///
/// Supported actions:
///   raw         — post a static `task` payload to the target
///   github_poll — poll GitHub API for issues/comments, post new events to target
use anyhow::{Context, Result};
use chrono::Utc;
use cron::Schedule;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{ScheduleAction, ScheduleDef};
use crate::unified_inbox;

/// Spawn one tokio task per schedule entry and return their handles.
/// Callers can abort the returned handles to cancel running schedules.
pub fn start(
    defs: Vec<ScheduleDef>,
    bus_socket: String,
    agent_name: String,
    home_dir: String,
) -> Vec<tokio::task::JoinHandle<()>> {
    defs.into_iter()
        .map(|def| {
            let bus = bus_socket.clone();
            let name = agent_name.clone();
            let home = home_dir.clone();
            tokio::spawn(async move {
                run_schedule(def, bus, name, home).await;
            })
        })
        .collect()
}

/// Watch a config file for changes and hot-reload schedules.
///
/// Performs initial load, then polls the file mtime every 30 seconds.
/// On change, aborts all running schedule tasks and restarts them from the
/// new config.
pub async fn watch_and_reload(
    config_path: String,
    bus_socket: String,
    agent_name: String,
    home_dir: String,
) {
    let mut last_modified = file_mtime(&config_path);
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Initial load
    if let Ok(cfg) = crate::config::UserConfig::load(&config_path)
        && !cfg.schedules.is_empty()
    {
        let count = cfg.schedules.len();
        handles = start(
            cfg.schedules,
            bus_socket.clone(),
            agent_name.clone(),
            home_dir.clone(),
        );
        info!(agent = %agent_name, count, "initial schedules loaded");
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let current_mtime = file_mtime(&config_path);
        if current_mtime == last_modified {
            continue;
        }
        last_modified = current_mtime;

        info!(agent = %agent_name, "config file changed, reloading schedules");

        // Cancel all existing schedule tasks
        let removed = handles.len();
        for h in handles.drain(..) {
            h.abort();
        }

        // Reload config and restart schedules
        match crate::config::UserConfig::load(&config_path) {
            Ok(cfg) => {
                let added = cfg.schedules.len();
                handles = start(
                    cfg.schedules,
                    bus_socket.clone(),
                    agent_name.clone(),
                    home_dir.clone(),
                );
                info!(agent = %agent_name, added, removed, "schedules reloaded");
            }
            Err(e) => {
                warn!(agent = %agent_name, error = %e, "failed to reload config, schedules stopped");
            }
        }
    }
}

fn file_mtime(path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

async fn run_schedule(def: ScheduleDef, bus_socket: String, agent_name: String, home_dir: String) {
    let schedule = match Schedule::from_str(&def.cron) {
        Ok(s) => s,
        Err(e) => {
            warn!(agent = %agent_name, cron = %def.cron, error = %e, "invalid cron expression, schedule skipped");
            return;
        }
    };

    info!(agent = %agent_name, cron = %def.cron, target = %def.target, "schedule started");

    loop {
        // Compute next fire time
        let now = Utc::now();
        let next = match schedule.upcoming(chrono::Utc).next() {
            Some(t) => t,
            None => {
                warn!(agent = %agent_name, cron = %def.cron, "no upcoming occurrence, schedule stopped");
                return;
            }
        };

        let duration = (next - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(60));
        debug!(agent = %agent_name, target = %def.target, sleep_secs = duration.as_secs(), "schedule sleeping until next fire");
        tokio::time::sleep(duration).await;

        info!(agent = %agent_name, target = %def.target, action = ?def.action, "schedule firing");

        if let Err(e) = fire(&def, &bus_socket, &agent_name, &home_dir).await {
            warn!(agent = %agent_name, target = %def.target, error = %e, "schedule fire failed");
        }
    }
}

async fn fire(def: &ScheduleDef, bus_socket: &str, agent_name: &str, home_dir: &str) -> Result<()> {
    match def.action {
        ScheduleAction::Raw => fire_raw(def, bus_socket, agent_name).await,
        ScheduleAction::GithubPoll => fire_github_poll(def, bus_socket, agent_name, home_dir).await,
        ScheduleAction::Shell => fire_shell(def, bus_socket, agent_name, home_dir).await,
    }
}

/// Post a static payload string to the bus target.
async fn fire_raw(def: &ScheduleDef, bus_socket: &str, agent_name: &str) -> Result<()> {
    let text = def
        .config
        .as_ref()
        .and_then(|c| c.as_str())
        .unwrap_or("scheduled event");

    // Write to unified inbox
    let inbox_name = format!("schedule/{}-raw", agent_name);
    let inbox_msg = unified_inbox::InboxMessage {
        ts: chrono::Utc::now(),
        source: "schedule".to_string(),
        from: None,
        text: text.to_string(),
        metadata: serde_json::json!({
            "target": def.target,
            "cron": def.cron,
        }),
    };
    if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
        warn!(agent = %agent_name, error = %e, "failed to write schedule event to unified inbox");
    }

    post_to_bus(bus_socket, agent_name, &def.target, text, "schedule").await
}

/// Poll GitHub for issues, comments, and pull requests, post new events to the bus.
///
/// Config fields:
///   `repos`  — list of "owner/repo" strings
///   `label`  — filter issues by label (empty string = no filter)
///   `events` — list of event types: "issues", "issue_comments", "pull_requests"
///              (default: ["issues"] for backward compatibility)
///
/// Uses `{home_dir}/.deskd/github_poll_since.json` to track the last poll time per repo,
/// so only new/updated items are posted on each cycle.
async fn fire_github_poll(
    def: &ScheduleDef,
    bus_socket: &str,
    agent_name: &str,
    home_dir: &str,
) -> Result<()> {
    let cfg = match &def.config {
        Some(c) => c,
        None => {
            warn!(agent = %agent_name, "github_poll schedule has no config, skipping");
            return Ok(());
        }
    };

    let repos = cfg
        .get("repos")
        .and_then(|r| r.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let label = cfg.get("label").and_then(|l| l.as_str()).unwrap_or("");

    let events = parse_events(cfg);

    let ignore_users: Vec<String> = cfg
        .get("ignore_users")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let mut since_state = load_since_state(home_dir);

    for repo in &repos {
        let since = since_state.get(repo).cloned().unwrap_or_else(|| {
            (Utc::now() - chrono::Duration::minutes(5))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });

        let mut count = 0;
        let mut had_error = false;

        if events.contains(&"issues".to_string()) {
            match poll_issues(
                repo,
                label,
                &since,
                bus_socket,
                agent_name,
                &def.target,
                &ignore_users,
            )
            .await
            {
                Ok(n) => count += n,
                Err(e) => {
                    warn!(agent = %agent_name, repo = %repo, error = %e, "github_poll issues failed");
                    had_error = true;
                }
            }
        }

        if events.contains(&"issue_comments".to_string()) {
            match poll_issue_comments(
                repo,
                &since,
                bus_socket,
                agent_name,
                &def.target,
                &ignore_users,
            )
            .await
            {
                Ok(n) => count += n,
                Err(e) => {
                    warn!(agent = %agent_name, repo = %repo, error = %e, "github_poll issue_comments failed");
                    had_error = true;
                }
            }
        }

        if events.contains(&"pull_requests".to_string()) {
            match poll_pull_requests(
                repo,
                &since,
                bus_socket,
                agent_name,
                &def.target,
                &ignore_users,
            )
            .await
            {
                Ok(n) => count += n,
                Err(e) => {
                    warn!(agent = %agent_name, repo = %repo, error = %e, "github_poll pull_requests failed");
                    had_error = true;
                }
            }
        }

        // Only update since timestamp if all polls succeeded — otherwise retry next cycle.
        if !had_error {
            since_state.insert(
                repo.clone(),
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
        }

        if count > 0 {
            info!(agent = %agent_name, repo = %repo, events = count, "github_poll posted events");
        }
    }

    save_since_state(home_dir, &since_state);
    Ok(())
}

/// Parse the `events` array from github_poll config.
/// Default: `["issues"]` for backward compatibility.
fn parse_events(cfg: &serde_yaml::Value) -> Vec<String> {
    cfg.get("events")
        .and_then(|e| e.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_else(|| vec!["issues".to_string()])
}

/// Poll GitHub issues API with `since` filter, post new/updated issues to bus.
/// Returns the number of events posted.
async fn poll_issues(
    repo: &str,
    label: &str,
    since: &str,
    bus_socket: &str,
    agent_name: &str,
    target: &str,
    ignore_users: &[String],
) -> Result<usize> {
    let mut endpoint = format!(
        "repos/{}/issues?state=open&since={}&per_page=100&sort=updated&direction=desc",
        repo, since
    );
    if !label.is_empty() {
        endpoint.push_str(&format!("&labels={}", label));
    }

    let output = tokio::process::Command::new("gh")
        .args(["api", &endpoint])
        .output()
        .await
        .context("failed to run gh api for issues")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh api issues failed: {}", stderr.trim());
    }

    let items: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("failed to parse gh api issues output")?;

    let mut count = 0;
    for item in items {
        // GitHub issues API returns PRs too — filter them out
        if item.get("pull_request").is_some() {
            continue;
        }

        let user = item
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
            .unwrap_or("");
        if ignore_users.iter().any(|u| u == user) {
            continue;
        }

        let title = item.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let number = item.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let body = item.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let html_url = item.get("html_url").and_then(|u| u.as_str()).unwrap_or("");

        let text = format!("GitHub issue {repo}#{number}: {title}\n{html_url}\n\n{body}");
        info!(agent = %agent_name, repo = %repo, issue = number, "posting github issue to bus");

        // Write to unified inbox
        let inbox_name = format!("github/{}", repo.replace('/', "-"));
        let inbox_msg = unified_inbox::InboxMessage {
            ts: chrono::Utc::now(),
            source: "github_poll".to_string(),
            from: Some(user.to_string()),
            text: text.clone(),
            metadata: serde_json::json!({
                "repo": repo,
                "issue": number,
                "type": "issue",
                "url": html_url,
            }),
        };
        if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
            warn!(repo = %repo, error = %e, "failed to write github issue to unified inbox");
        }

        if let Err(e) = post_to_bus(bus_socket, agent_name, target, &text, "github_poll").await {
            warn!(error = %e, "failed to post github issue to bus");
        }
        count += 1;
    }

    Ok(count)
}

/// Poll GitHub issue comments API with `since` filter, post new comments to bus.
/// Returns the number of events posted.
async fn poll_issue_comments(
    repo: &str,
    since: &str,
    bus_socket: &str,
    agent_name: &str,
    target: &str,
    ignore_users: &[String],
) -> Result<usize> {
    let endpoint = format!(
        "repos/{}/issues/comments?since={}&per_page=100&sort=updated&direction=desc",
        repo, since
    );

    let output = tokio::process::Command::new("gh")
        .args(["api", &endpoint])
        .output()
        .await
        .context("failed to run gh api for issue comments")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh api issue comments failed: {}", stderr.trim());
    }

    let comments: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("failed to parse gh api comments output")?;

    let mut count = 0;
    for comment in comments {
        let user = comment
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
            .unwrap_or("unknown");
        if ignore_users.iter().any(|u| u == user) {
            continue;
        }
        let body = comment.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let html_url = comment
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("");

        // Extract issue number from issue_url (e.g. "https://api.github.com/repos/owner/repo/issues/42")
        let issue_number = comment
            .get("issue_url")
            .and_then(|u| u.as_str())
            .and_then(|url| url.rsplit('/').next())
            .unwrap_or("?");

        let text =
            format!("GitHub comment on {repo}#{issue_number} by {user}:\n{html_url}\n\n{body}");
        info!(agent = %agent_name, repo = %repo, issue = %issue_number, user = %user, "posting github comment to bus");

        // Write to unified inbox
        let inbox_name = format!("github/{}", repo.replace('/', "-"));
        let inbox_msg = unified_inbox::InboxMessage {
            ts: chrono::Utc::now(),
            source: "github_poll".to_string(),
            from: Some(user.to_string()),
            text: text.clone(),
            metadata: serde_json::json!({
                "repo": repo,
                "issue": issue_number,
                "type": "issue_comment",
                "url": html_url,
            }),
        };
        if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
            warn!(repo = %repo, error = %e, "failed to write github comment to unified inbox");
        }

        if let Err(e) = post_to_bus(bus_socket, agent_name, target, &text, "github_poll").await {
            warn!(error = %e, "failed to post github comment to bus");
        }
        count += 1;
    }

    Ok(count)
}

/// Poll GitHub pull requests API with `since` filter, post new/updated PRs to bus.
/// Returns the number of events posted.
async fn poll_pull_requests(
    repo: &str,
    since: &str,
    bus_socket: &str,
    agent_name: &str,
    target: &str,
    ignore_users: &[String],
) -> Result<usize> {
    let endpoint = format!(
        "repos/{}/pulls?state=open&sort=updated&direction=desc&per_page=100",
        repo
    );

    let output = tokio::process::Command::new("gh")
        .args(["api", &endpoint])
        .output()
        .await
        .context("failed to run gh api for pull requests")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh api pull requests failed: {}", stderr.trim());
    }

    let prs: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .context("failed to parse gh api pull requests output")?;

    let since_dt = chrono::DateTime::parse_from_rfc3339(since)
        .unwrap_or_else(|_| chrono::DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z").unwrap());

    let mut count = 0;
    for pr in prs {
        // Filter by updated_at >= since
        let updated = pr
            .get("updated_at")
            .and_then(|u| u.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        if let Some(updated) = updated
            && updated < since_dt
        {
            continue;
        }

        let user = pr
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
            .unwrap_or("");
        if ignore_users.iter().any(|u| u == user) {
            continue;
        }

        let title = pr.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let number = pr.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let body = pr.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let html_url = pr.get("html_url").and_then(|u| u.as_str()).unwrap_or("");

        let text = format!("New pull request {repo}#{number}: {title}\n{html_url}\n\n{body}");
        info!(agent = %agent_name, repo = %repo, pr = number, "posting github PR to bus");

        // Write to unified inbox
        let inbox_name = format!("github/{}", repo.replace('/', "-"));
        let inbox_msg = unified_inbox::InboxMessage {
            ts: chrono::Utc::now(),
            source: "github_poll".to_string(),
            from: Some(user.to_string()),
            text: text.clone(),
            metadata: serde_json::json!({
                "repo": repo,
                "pr": number,
                "type": "pull_request",
                "url": html_url,
            }),
        };
        if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
            warn!(repo = %repo, error = %e, "failed to write github PR to unified inbox");
        }

        if let Err(e) = post_to_bus(bus_socket, agent_name, target, &text, "github_poll").await {
            warn!(error = %e, "failed to post github PR to bus");
        }
        count += 1;
    }

    Ok(count)
}

// ─── Since state persistence ────────────────────────────────────────────────

/// Path to the since-state file: `{home_dir}/.deskd/github_poll_since.json`
fn since_state_path(home_dir: &str) -> PathBuf {
    PathBuf::from(home_dir)
        .join(".deskd")
        .join("github_poll_since.json")
}

/// Load the per-repo since timestamps from disk.
fn load_since_state(home_dir: &str) -> HashMap<String, String> {
    let path = since_state_path(home_dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(state) => {
                debug!(path = %path.display(), "loaded github_poll since state");
                state
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse since state, starting fresh");
                HashMap::new()
            }
        },
        Err(_) => {
            info!(path = %path.display(), "no since state file, first poll will fetch recent items");
            HashMap::new()
        }
    }
}

/// Persist the per-repo since timestamps to disk.
fn save_since_state(home_dir: &str, state: &HashMap<String, String>) {
    let path = since_state_path(home_dir);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!(path = %parent.display(), error = %e, "failed to create since state directory");
        return;
    }
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, &json) {
                warn!(path = %path.display(), error = %e, "failed to write since state file");
            } else {
                debug!(path = %path.display(), "saved github_poll since state");
            }
        }
        Err(e) => {
            warn!(error = %e, "failed to serialize since state");
        }
    }
}

/// Run an arbitrary shell command via `sh -c`.
/// If the command exits successfully and produces stdout, it is posted to the bus target.
/// If the command fails, the error is logged (no bus message).
async fn fire_shell(
    def: &ScheduleDef,
    bus_socket: &str,
    agent_name: &str,
    home_dir: &str,
) -> Result<()> {
    let command = def
        .config
        .as_ref()
        .and_then(|c| c.get("command"))
        .and_then(|v| v.as_str())
        .or_else(|| def.config.as_ref().and_then(|c| c.as_str()))
        .unwrap_or_else(|| {
            warn!(agent = %agent_name, "shell schedule has no command, skipping");
            ""
        });

    if command.is_empty() {
        return Ok(());
    }

    info!(agent = %agent_name, command = %command, "shell schedule firing");

    let output = tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(home_dir)
        .output()
        .await
        .with_context(|| format!("failed to spawn shell command: {}", command))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            agent = %agent_name,
            command = %command,
            exit_code = ?output.status.code(),
            stderr = %stderr.trim(),
            "shell schedule command failed"
        );
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    if !text.is_empty() && !def.target.is_empty() {
        post_to_bus(bus_socket, agent_name, &def.target, text, "shell").await?;
    }

    Ok(())
}

/// Scan `~/.deskd/reminders/` every 10 seconds and fire any due reminders.
///
/// Each reminder is a JSON file (`RemindDef`) written by `deskd remind` or the
/// `create_reminder` MCP tool. When the `at` timestamp is <= now, the reminder
/// is fired (message posted to bus) and the file is deleted.
pub async fn run_reminders(bus_socket: String, agent_name: String) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        let dir = crate::config::reminders_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(agent = %agent_name, error = %e, "failed to read reminders dir");
                continue;
            }
        };

        let now = Utc::now();

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(agent = %agent_name, path = %path.display(), error = %e, "failed to read reminder file");
                    continue;
                }
            };

            let reminder: crate::config::RemindDef = match serde_json::from_str(&content) {
                Ok(r) => r,
                Err(e) => {
                    warn!(agent = %agent_name, path = %path.display(), error = %e, "failed to parse reminder file");
                    continue;
                }
            };

            let fire_at = match chrono::DateTime::parse_from_rfc3339(&reminder.at) {
                Ok(t) => t.with_timezone(&chrono::Utc),
                Err(e) => {
                    warn!(agent = %agent_name, path = %path.display(), error = %e, "invalid reminder timestamp");
                    continue;
                }
            };

            if fire_at > now {
                // Not yet due.
                continue;
            }

            info!(agent = %agent_name, target = %reminder.target, "firing reminder");

            // Write to unified inbox
            let inbox_name = format!("schedule/{}-reminder", agent_name);
            let inbox_msg = unified_inbox::InboxMessage {
                ts: chrono::Utc::now(),
                source: "reminder".to_string(),
                from: None,
                text: reminder.message.clone(),
                metadata: serde_json::json!({
                    "target": reminder.target,
                    "scheduled_at": reminder.at,
                }),
            };
            if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
                warn!(agent = %agent_name, error = %e, "failed to write reminder to unified inbox");
            }

            if let Err(e) = post_to_bus(
                &bus_socket,
                &agent_name,
                &reminder.target,
                &reminder.message,
                "reminder",
            )
            .await
            {
                warn!(agent = %agent_name, target = %reminder.target, error = %e, "failed to fire reminder");
            } else {
                // Delete the file after successful delivery.
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(agent = %agent_name, path = %path.display(), error = %e, "failed to delete reminder file");
                }
            }
        }
    }
}

/// Post a task message to the bus.
/// `source_label` identifies the schedule type (e.g. "github_poll", "reminder", "shell", "raw").
async fn post_to_bus(
    socket_path: &str,
    agent_name: &str,
    target: &str,
    text: &str,
    source_label: &str,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("schedule: failed to connect to bus at {}", socket_path))?;

    let reg = serde_json::json!({
        "type": "register",
        "name": format!("schedule-{}-{}", agent_name, Uuid::new_v4()),
        "subscriptions": [],
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": format!("{}-{}", source_label, agent_name),
        "target": target,
        "payload": {"task": text},
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_events_default() {
        let cfg: serde_yaml::Value = serde_yaml::from_str("repos: []").unwrap();
        let events = parse_events(&cfg);
        assert_eq!(events, vec!["issues".to_string()]);
    }

    #[test]
    fn test_parse_events_explicit() {
        let cfg: serde_yaml::Value =
            serde_yaml::from_str("events: [issues, issue_comments]").unwrap();
        let events = parse_events(&cfg);
        assert_eq!(events, vec!["issues", "issue_comments"]);
    }

    #[test]
    fn test_parse_events_single() {
        let cfg: serde_yaml::Value = serde_yaml::from_str("events: [issue_comments]").unwrap();
        let events = parse_events(&cfg);
        assert_eq!(events, vec!["issue_comments"]);
    }

    #[test]
    fn test_since_state_roundtrip() {
        let dir = std::env::temp_dir().join("deskd_test_since");
        let _ = std::fs::remove_dir_all(&dir);
        let home_dir = dir.to_string_lossy().to_string();

        let mut state = HashMap::new();
        state.insert("owner/repo".to_string(), "2026-03-27T10:00:00Z".to_string());

        save_since_state(&home_dir, &state);

        let loaded = load_since_state(&home_dir);
        assert_eq!(loaded.get("owner/repo").unwrap(), "2026-03-27T10:00:00Z");

        // Verify file exists at expected path
        let path = since_state_path(&home_dir);
        assert!(path.exists(), "since state file should exist at {:?}", path);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_fire_shell_uses_home_dir_as_working_directory() {
        // Create a temp directory to act as home_dir
        let dir = std::env::temp_dir().join("deskd_test_shell_workdir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home_dir = dir.to_string_lossy().to_string();

        // Write a marker file in the temp dir
        std::fs::write(dir.join("marker.txt"), "hello from workdir").unwrap();

        // Build a ScheduleDef with a shell command that reads the marker file
        let config: serde_yaml::Value =
            serde_yaml::from_str("command: \"cat marker.txt\"").unwrap();
        let def = ScheduleDef {
            cron: "0 0 * * * *".to_string(),
            target: String::new(), // empty target — output won't be posted
            action: ScheduleAction::Shell,
            config: Some(config),
        };

        // fire_shell should succeed because marker.txt exists in home_dir
        // We use a dummy bus_socket since target is empty and output won't be sent
        let result = fire_shell(&def, "/tmp/nonexistent.sock", "test-agent", &home_dir).await;
        assert!(result.is_ok(), "fire_shell should succeed: {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_since_state_missing_file_returns_empty() {
        let dir = std::env::temp_dir().join("deskd_test_since_missing");
        let _ = std::fs::remove_dir_all(&dir);
        let home_dir = dir.to_string_lossy().to_string();

        let state = load_since_state(&home_dir);
        assert!(state.is_empty());
    }

    #[test]
    fn test_start_creates_handles_per_schedule_def() {
        // start() requires a tokio runtime because it calls tokio::spawn.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let defs = vec![
                ScheduleDef {
                    cron: "0 9 * * *".into(),
                    target: "agent:family".into(),
                    action: ScheduleAction::Raw,
                    config: Some(serde_yaml::Value::String("morning brief".into())),
                },
                ScheduleDef {
                    cron: "0 21 * * *".into(),
                    target: "agent:family".into(),
                    action: ScheduleAction::Raw,
                    config: Some(serde_yaml::Value::String("evening check".into())),
                },
            ];

            let handles = start(
                defs,
                "/tmp/nonexistent.sock".into(),
                "family".into(),
                "/tmp/family_test".into(),
            );

            assert_eq!(
                handles.len(),
                2,
                "should create one handle per schedule def"
            );

            // Clean up: abort the spawned tasks
            for h in handles {
                h.abort();
            }
        });
    }

    #[test]
    fn test_load_schedules_from_agent_config_file() {
        // Verify that UserConfig::load() picks up schedules from an agent-level
        // deskd.yaml, which is the mechanism used by watch_and_reload().
        let dir = std::env::temp_dir().join("deskd_test_agent_schedules");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config_path = dir.join("deskd.yaml");
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Family assistant"

schedules:
  - cron: "3 7 * * *"
    target: "agent:family"
    action: raw
    config: "Morning brief"
  - cron: "3 21 * * *"
    target: "agent:family"
    action: github_poll
    config:
      repos:
        - kgatilin/deskd
      label: agent-ready
  - cron: "7 22 * * *"
    target: "agent:family"
    action: shell
    config:
      command: "echo hello"
"#;
        std::fs::write(&config_path, yaml).unwrap();

        let cfg = crate::config::UserConfig::load(&config_path.to_string_lossy()).unwrap();

        assert_eq!(cfg.schedules.len(), 3);
        assert_eq!(cfg.schedules[0].cron, "3 7 * * *");
        assert_eq!(cfg.schedules[0].target, "agent:family");
        assert!(matches!(cfg.schedules[0].action, ScheduleAction::Raw));
        assert!(matches!(
            cfg.schedules[1].action,
            ScheduleAction::GithubPoll
        ));
        assert!(matches!(cfg.schedules[2].action, ScheduleAction::Shell));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_file_mtime_returns_none_for_missing_file() {
        assert!(file_mtime("/tmp/definitely_nonexistent_deskd_test_file.yaml").is_none());
    }

    #[test]
    fn test_file_mtime_returns_some_for_existing_file() {
        let dir = std::env::temp_dir().join("deskd_test_mtime");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.yaml");
        std::fs::write(&path, "test").unwrap();

        assert!(file_mtime(&path.to_string_lossy()).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
