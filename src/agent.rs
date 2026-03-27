use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, UserConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub work_dir: String,
    pub max_turns: u32,
    /// Optional Linux user to run the agent process as.
    #[serde(default)]
    pub unix_user: Option<String>,
    /// Budget cap in USD.
    #[serde(default = "default_budget_usd")]
    pub budget_usd: f64,
    /// Command to run. Defaults to ["claude"].
    #[serde(default = "default_agent_command")]
    pub command: Vec<String>,
    /// Path to the agent's deskd.yaml (injected as DESKD_AGENT_CONFIG into claude).
    #[serde(default)]
    pub config_path: Option<String>,
}

fn default_budget_usd() -> f64 {
    50.0
}

fn default_agent_command() -> Vec<String> {
    vec!["claude".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub config: AgentConfig,
    pub pid: u32,
    pub session_id: String,
    pub total_turns: u32,
    pub total_cost: f64,
    pub created_at: String,
    /// "idle" or "working"
    #[serde(default = "default_status")]
    pub status: String,
    /// Truncated text of the task currently being processed.
    #[serde(default)]
    pub current_task: String,
}

fn default_status() -> String {
    "idle".to_string()
}

fn state_path(name: &str) -> PathBuf {
    config::state_dir().join(format!("{}.yaml", name))
}

fn log_path(name: &str) -> PathBuf {
    config::log_dir().join(format!("{}.log", name))
}

pub fn load_state(name: &str) -> Result<AgentState> {
    let path = state_path(name);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("Agent '{}' not found", name))?;
    let state: AgentState = serde_yaml::from_str(&content)?;
    Ok(state)
}

fn save_state(state: &AgentState) -> Result<()> {
    let path = state_path(&state.config.name);
    let content = serde_yaml::to_string(state)?;
    std::fs::write(&path, content)?;
    Ok(())
}

pub fn save_state_pub(state: &AgentState) -> Result<()> {
    save_state(state)
}

/// Create a new agent (saves state file; does not start a worker process).
pub async fn create(cfg: &AgentConfig) -> Result<AgentState> {
    if state_path(&cfg.name).exists() {
        bail!("Agent '{}' already exists. Remove it first.", cfg.name);
    }

    let state = AgentState {
        config: cfg.clone(),
        pid: 0,
        session_id: String::new(),
        total_turns: 0,
        total_cost: 0.0,
        created_at: Utc::now().to_rfc3339(),
        status: "idle".to_string(),
        current_task: String::new(),
    };

    save_state(&state)?;
    info!(agent = %cfg.name, "agent created");
    Ok(state)
}

/// Create or update agent state from an AgentConfig.
/// If state already exists, updates the config fields but preserves session/cost/turns.
/// Used for sub-agents spawned from deskd.yaml.
pub async fn create_or_update_from_config(cfg: &AgentConfig) -> Result<AgentState> {
    let path = state_path(&cfg.name);
    if path.exists() {
        let mut state = load_state(&cfg.name)?;
        state.config = cfg.clone();
        save_state(&state)?;
        info!(agent = %cfg.name, "sub-agent state updated");
        return Ok(state);
    }
    let state = AgentState {
        config: cfg.clone(),
        pid: 0,
        session_id: String::new(),
        total_turns: 0,
        total_cost: 0.0,
        created_at: Utc::now().to_rfc3339(),
        status: "idle".to_string(),
        current_task: String::new(),
    };
    save_state(&state)?;
    info!(agent = %cfg.name, "sub-agent created");
    Ok(state)
}

/// Create or recover agent state from workspace AgentDef + optional UserConfig.
/// If state already exists, returns it with config fields updated from current def.
/// Model priority: workspace def.model override > user_cfg.model > default.
pub async fn create_or_recover(
    def: &config::AgentDef,
    user_cfg: Option<&UserConfig>,
) -> Result<AgentState> {
    let model = def
        .model
        .clone()
        .or_else(|| user_cfg.map(|c| c.model.clone()))
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    let system_prompt = user_cfg
        .map(|c| c.system_prompt.clone())
        .unwrap_or_default();

    let max_turns = user_cfg.map(|c| c.max_turns).unwrap_or(100);

    let cfg = AgentConfig {
        name: def.name.clone(),
        model,
        system_prompt,
        work_dir: def.work_dir.clone(),
        max_turns,
        unix_user: def.unix_user.clone(),
        budget_usd: def.budget_usd,
        command: def.command.clone(),
        config_path: Some(def.config_path()),
    };

    let path = state_path(&def.name);
    if path.exists() {
        let mut state = load_state(&def.name)?;
        // Update mutable fields on recovery; preserve session_id + costs.
        state.config = cfg;
        save_state(&state)?;
        info!(agent = %def.name, session_id = %state.session_id, "recovered existing agent state");
        return Ok(state);
    }

    let state = AgentState {
        config: cfg,
        pid: 0,
        session_id: String::new(),
        total_turns: 0,
        total_cost: 0.0,
        created_at: Utc::now().to_rfc3339(),
        status: "idle".to_string(),
        current_task: String::new(),
    };
    save_state(&state)?;
    info!(agent = %def.name, "agent created");
    Ok(state)
}

/// Run claude for one task, return the response text.
///
/// `bus_socket`: path to the agent's bus (injected as DESKD_BUS_SOCKET).
/// Claude uses this to call the `send_message` MCP tool.
pub async fn send(
    name: &str,
    message: &str,
    max_turns: Option<u32>,
    bus_socket: Option<&str>,
) -> Result<String> {
    send_inner(name, message, max_turns, bus_socket, None).await
}

/// Like `send`, but streams each assistant text block to `progress_tx` as it arrives.
/// Still returns the full concatenated response when done.
pub async fn send_streaming(
    name: &str,
    message: &str,
    max_turns: Option<u32>,
    bus_socket: Option<&str>,
    progress_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<String> {
    send_inner(name, message, max_turns, bus_socket, Some(progress_tx)).await
}

async fn send_inner(
    name: &str,
    message: &str,
    max_turns: Option<u32>,
    bus_socket: Option<&str>,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<String> {
    let mut state = load_state(name)?;

    let turns = max_turns.unwrap_or(state.config.max_turns);

    // Dynamic args only — the base command + flags come from config.command field.
    // deskd appends session management and the prompt.
    let mut args: Vec<String> = Vec::new();

    if !state.session_id.is_empty() {
        args.push("--resume".to_string());
        args.push(state.session_id.clone());
    }

    if !state.config.system_prompt.is_empty() && state.session_id.is_empty() {
        args.push("--system-prompt".to_string());
        args.push(state.config.system_prompt.clone());
    }

    // -p <message> goes last so leading dashes in message are not parsed as flags.
    args.push("-p".to_string());
    args.push(message.to_string());

    debug!(agent = %name, turns, "spawning claude");

    // Env vars injected into the claude process:
    //   DESKD_BUS_SOCKET  — bus socket for MCP send_message tool
    //   DESKD_AGENT_NAME  — agent name for MCP server
    //   DESKD_AGENT_CONFIG — path to deskd.yaml for tool description generation
    let bus_path = bus_socket
        .map(|s| s.to_string())
        .unwrap_or_else(|| config::agent_bus_socket(&state.config.work_dir));

    let config_path_str = state.config.config_path.clone().unwrap_or_default();
    let mut extra_env: Vec<(&str, &str)> =
        vec![("DESKD_AGENT_NAME", name), ("DESKD_BUS_SOCKET", &bus_path)];
    if !config_path_str.is_empty() {
        extra_env.push(("DESKD_AGENT_CONFIG", &config_path_str));
    }

    let mut cmd = build_command(&state.config, &args, &extra_env);
    let mut child = cmd.spawn().context("Failed to spawn claude CLI")?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // Read stderr in background so it doesn't block stdout parsing.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = tokio::io::BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf).await;
        buf
    });

    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut response_text = String::new();
    let mut new_session_id = String::new();
    let mut task_cost = 0.0;
    let mut task_turns = 0u32;

    while let Some(line) = lines.next_line().await? {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("assistant") => {
                    let mut block_text = String::new();
                    if let Some(blocks) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for block in blocks {
                            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                                && let Some(text) = block.get("text").and_then(|t| t.as_str())
                            {
                                block_text.push_str(text);
                            }
                        }
                    }
                    if !block_text.is_empty() {
                        if let Some(tx) = &progress_tx {
                            let _ = tx.send(block_text.clone());
                        }
                        response_text.push_str(&block_text);
                    }
                }
                Some("result") => {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        new_session_id = sid.to_string();
                    }
                    if let Some(cost) = v.get("total_cost_usd").and_then(|c| c.as_f64()) {
                        task_cost = cost;
                    }
                    if let Some(t) = v.get("num_turns").and_then(|t| t.as_u64()) {
                        task_turns = t as u32;
                    }
                }
                _ => {}
            }
        }
    }

    let _ = child.wait().await;
    let stderr_str = stderr_task.await.unwrap_or_default();

    if !new_session_id.is_empty() {
        state.session_id = new_session_id;
    }
    state.total_cost += task_cost;
    state.total_turns += task_turns;
    save_state(&state)?;

    if response_text.is_empty() {
        if !stderr_str.is_empty() {
            bail!("Claude error: {}", stderr_str.trim());
        }
        // If we used --resume and got no response, the session_id is stale.
        // Clear it and retry once with a fresh session.
        if !state.session_id.is_empty() {
            warn!(agent = %name, session_id = %state.session_id, "stale session_id — retrying without --resume");
            state.session_id = String::new();
            save_state(&state)?;
            return Box::pin(send_inner(
                name,
                message,
                max_turns,
                bus_socket,
                progress_tx,
            ))
            .await;
        }
        bail!("No response from claude");
    }

    Ok(response_text)
}

/// Build the tokio Command for running the agent process.
/// Uses cfg.command as the executable (defaults to ["claude"]).
/// When unix_user is set, wraps with sudo and strips SSH env vars.
/// extra_env: additional environment variables to pass to the process.
pub fn build_command(cfg: &AgentConfig, args: &[String], extra_env: &[(&str, &str)]) -> Command {
    let (bin, prefix) = split_command(&cfg.command);
    let mut cmd = match &cfg.unix_user {
        Some(user) => {
            let mut c = Command::new("sudo");
            c.args(["-u", user, "-H", "--"]);
            c.arg(bin);
            c.args(prefix);
            c.args(args);
            c.env_remove("SSH_AUTH_SOCK");
            c.env_remove("SSH_AGENT_PID");
            c
        }
        None => {
            let mut c = Command::new(bin);
            c.args(prefix);
            c.args(args);
            c
        }
    };
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.current_dir(&cfg.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn split_command(command: &[String]) -> (&str, &[String]) {
    match command {
        [] => ("claude", &[]),
        [bin] => (bin.as_str(), &[]),
        [bin, rest @ ..] => (bin.as_str(), rest),
    }
}

/// Spawn an ephemeral sub-agent: create, run task, clean up.
/// Returns the agent's text response.
pub async fn spawn_ephemeral(
    name: &str,
    task: &str,
    model: &str,
    work_dir: &str,
    max_turns: u32,
    bus_socket: &str,
    parent_name: &str,
) -> Result<String> {
    let unique_name = format!("{}-{}", name, uuid::Uuid::new_v4().as_simple());

    let cfg = AgentConfig {
        name: unique_name.clone(),
        model: model.to_string(),
        system_prompt: String::new(),
        work_dir: work_dir.to_string(),
        max_turns,
        unix_user: None,
        budget_usd: 50.0,
        command: default_agent_command(),
        config_path: None,
    };

    create(&cfg).await?;
    info!(agent = %unique_name, parent = %parent_name, bus = %bus_socket, "spawning ephemeral sub-agent");

    let result = send(&unique_name, task, Some(max_turns), Some(bus_socket)).await;

    if let Err(e) = remove(&unique_name).await {
        warn!(agent = %unique_name, error = %e, "failed to clean up ephemeral agent");
    }

    result
}

/// List all agents whose state files exist on disk.
pub async fn list() -> Result<Vec<AgentState>> {
    let dir = config::state_dir();
    let mut agents = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "yaml").unwrap_or(false)
                && let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(state) = serde_yaml::from_str::<AgentState>(&content)
            {
                agents.push(state);
            }
        }
    }

    Ok(agents)
}

/// Remove an agent (state file + log).
pub async fn remove(name: &str) -> Result<()> {
    let path = state_path(name);
    if !path.exists() {
        bail!("Agent '{}' not found", name);
    }
    std::fs::remove_file(&path)?;
    let log = log_path(name);
    if log.exists()
        && let Err(e) = std::fs::remove_file(&log)
    {
        warn!(agent = %name, error = %e, "failed to remove log file");
    }
    info!(agent = %name, "agent removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config_defaults() {
        let yaml = r#"
config:
  name: test
  model: claude-opus-4-6
  system_prompt: ""
  work_dir: /tmp
  max_turns: 100
pid: 0
session_id: ""
total_turns: 0
total_cost: 0.0
created_at: "2024-01-01T00:00:00Z"
"#;
        let state: AgentState = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(state.config.budget_usd, 50.0);
        assert!(state.config.unix_user.is_none());
    }

    #[test]
    fn test_agent_state_round_trip() {
        let cfg = AgentConfig {
            name: "test-agent".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            system_prompt: String::new(),
            work_dir: "/tmp".to_string(),
            max_turns: 100,
            unix_user: Some("agent1".to_string()),
            budget_usd: 10.0,
            command: vec!["claude".to_string()],
            config_path: Some("/home/agent1/deskd.yaml".to_string()),
        };
        let state = AgentState {
            config: cfg,
            pid: 0,
            session_id: "sid-123".to_string(),
            total_turns: 5,
            total_cost: 0.42,
            created_at: Utc::now().to_rfc3339(),
            status: "idle".to_string(),
            current_task: String::new(),
        };
        let yaml = serde_yaml::to_string(&state).unwrap();
        let restored: AgentState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(restored.config.unix_user.as_deref(), Some("agent1"));
        assert_eq!(restored.session_id, "sid-123");
        assert_eq!(restored.total_cost, 0.42);
        assert_eq!(
            restored.config.config_path.as_deref(),
            Some("/home/agent1/deskd.yaml")
        );
    }
}
