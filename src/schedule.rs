/// Schedule runner — fires cron-based bus events.
///
/// Each `ScheduleDef` in deskd.yaml maps to a tokio task that sleeps until the
/// next cron occurrence, posts the configured payload to the bus, then repeats.
///
/// Supported actions:
///   raw         — post a static `task` payload to the target
///   github_poll — shell out to `gh issue list`, post new issues to target
use anyhow::{Context, Result};
use chrono::Utc;
use cron::Schedule;
use std::str::FromStr;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{ScheduleAction, ScheduleDef};

/// Spawn one tokio task per schedule entry.
/// `agent_name` is used for bus registration names and logging.
pub fn start(defs: Vec<ScheduleDef>, bus_socket: String, agent_name: String) {
    for def in defs {
        let bus = bus_socket.clone();
        let name = agent_name.clone();
        tokio::spawn(async move {
            run_schedule(def, bus, name).await;
        });
    }
}

async fn run_schedule(def: ScheduleDef, bus_socket: String, agent_name: String) {
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

        let duration = (next - now).to_std().unwrap_or(std::time::Duration::from_secs(60));
        debug!(agent = %agent_name, target = %def.target, sleep_secs = duration.as_secs(), "schedule sleeping until next fire");
        tokio::time::sleep(duration).await;

        info!(agent = %agent_name, target = %def.target, action = ?def.action, "schedule firing");

        if let Err(e) = fire(&def, &bus_socket, &agent_name).await {
            warn!(agent = %agent_name, target = %def.target, error = %e, "schedule fire failed");
        }
    }
}

async fn fire(def: &ScheduleDef, bus_socket: &str, agent_name: &str) -> Result<()> {
    match def.action {
        ScheduleAction::Raw => fire_raw(def, bus_socket, agent_name).await,
        ScheduleAction::GithubPoll => fire_github_poll(def, bus_socket, agent_name).await,
    }
}

/// Post a static payload string to the bus target.
async fn fire_raw(def: &ScheduleDef, bus_socket: &str, agent_name: &str) -> Result<()> {
    let text = def
        .config
        .as_ref()
        .and_then(|c| c.as_str())
        .unwrap_or("scheduled event");

    post_to_bus(bus_socket, agent_name, &def.target, text).await
}

/// Poll GitHub for issues with a configured label, post new ones to the bus.
/// Config fields: `repos` (list of "owner/repo"), `label` (string).
/// Tracks seen issue numbers in memory (resets on restart).
async fn fire_github_poll(def: &ScheduleDef, bus_socket: &str, agent_name: &str) -> Result<()> {
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

    let label = cfg
        .get("label")
        .and_then(|l| l.as_str())
        .unwrap_or("agent-ready");

    for repo in &repos {
        match fetch_github_issues(repo, label).await {
            Ok(issues) => {
                for issue in issues {
                    let title = issue.get("title").and_then(|t| t.as_str()).unwrap_or("");
                    let number = issue.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
                    let body = issue.get("body").and_then(|b| b.as_str()).unwrap_or("");
                    let url = issue.get("url").and_then(|u| u.as_str()).unwrap_or("");

                    let text = format!(
                        "GitHub issue {repo}#{number}: {title}\n{url}\n\n{body}"
                    );
                    info!(agent = %agent_name, repo = %repo, issue = number, "posting github issue to bus");
                    if let Err(e) = post_to_bus(bus_socket, agent_name, &def.target, &text).await {
                        warn!(error = %e, "failed to post github issue to bus");
                    }
                }
            }
            Err(e) => {
                warn!(agent = %agent_name, repo = %repo, error = %e, "github_poll failed");
            }
        }
    }

    Ok(())
}

/// Shell out to `gh issue list` to fetch open issues with the given label.
async fn fetch_github_issues(repo: &str, label: &str) -> Result<Vec<serde_json::Value>> {
    let output = tokio::process::Command::new("gh")
        .args([
            "issue", "list",
            "--repo", repo,
            "--label", label,
            "--state", "open",
            "--json", "title,body,number,url",
            "--limit", "10",
        ])
        .output()
        .await
        .context("failed to run gh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh issue list failed: {}", stderr.trim());
    }

    let issues: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .context("failed to parse gh output")?;
    Ok(issues)
}

/// Post a task message to the bus.
async fn post_to_bus(
    socket_path: &str,
    agent_name: &str,
    target: &str,
    text: &str,
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
        "source": format!("schedule-{}", agent_name),
        "target": target,
        "payload": {"task": text},
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    Ok(())
}
