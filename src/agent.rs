use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config;

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
    /// Command to run as the agent process. Defaults to ["claude"].
    #[serde(default = "default_agent_command")]
    pub command: Vec<String>,
}

fn default_agent_command() -> Vec<String> {
    vec!["claude".to_string()]
}

fn default_budget_usd() -> f64 {
    50.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub config: AgentConfig,
    pub pid: u32,
    pub session_id: String,
    pub total_turns: u32,
    pub total_cost: f64,
    pub created_at: String,
}

fn state_path(name: &str) -> PathBuf {
    config::state_dir().join(format!("{}.yaml", name))
}

fn log_path(name: &str) -> PathBuf {
    config::log_dir().join(format!("{}.log", name))
}

pub fn load_state(name: &str) -> Result<AgentState> {
    let path = state_path(name);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Agent '{}' not found", name))?;
    let state: AgentState = serde_yaml::from_str(&content)?;
    Ok(state)
}

fn save_state(state: &AgentState) -> Result<()> {
    let path = state_path(&state.config.name);
    let content = serde_yaml::to_string(state)?;
    std::fs::write(&path, content)?;
    Ok(())
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
    };

    save_state(&state)?;
    info!(agent = %cfg.name, "agent created");
    Ok(state)
}

/// Create or recover agent state from a workspace AgentDef.
/// If state already exists on disk, returns existing state (preserving session_id + costs).
pub async fn create_or_recover(def: &config::AgentDef) -> Result<AgentState> {
    let path = state_path(&def.name);
    if path.exists() {
        let state = load_state(&def.name)?;
        info!(agent = %def.name, session_id = %state.session_id, "recovered existing agent state");
        return Ok(state);
    }

    let cfg = AgentConfig {
        name: def.name.clone(),
        model: def.model.clone(),
        system_prompt: def.system_prompt.clone(),
        work_dir: def.work_dir.clone(),
        max_turns: def.max_turns,
        unix_user: def.unix_user.clone(),
        budget_usd: def.budget_usd,
        command: def.command.clone(),
    };
    create(&cfg).await
}

/// Send a message to an agent — runs claude CLI and returns the full response text.
pub async fn send(name: &str, message: &str, max_turns: Option<u32>) -> Result<String> {
    let mut state = load_state(name)?;

    let turns = max_turns.unwrap_or(state.config.max_turns);

    let mut args = vec![
        "-p".to_string(),
        message.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--model".to_string(),
        state.config.model.clone(),
        "--max-turns".to_string(),
        turns.to_string(),
    ];

    if !state.session_id.is_empty() {
        args.push("--resume".to_string());
        args.push(state.session_id.clone());
    }

    if !state.config.system_prompt.is_empty() && state.session_id.is_empty() {
        args.push("--system-prompt".to_string());
        args.push(state.config.system_prompt.clone());
    }

    debug!(agent = %name, turns, "spawning claude");

    let mut cmd = build_command(&state.config, &args);
    let output = cmd
        .output()
        .await
        .context("Failed to run claude CLI")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut response_text = String::new();
    let mut new_session_id = String::new();
    let mut task_cost = 0.0;
    let mut task_turns = 0u32;

    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("assistant") => {
                    if let Some(blocks) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for block in blocks {
                            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    response_text.push_str(text);
                                }
                            }
                        }
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

    if !new_session_id.is_empty() {
        state.session_id = new_session_id;
    }
    state.total_cost += task_cost;
    state.total_turns += task_turns;
    save_state(&state)?;

    if response_text.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            bail!("Claude error: {}", stderr.trim());
        }
        bail!("No response from claude");
    }

    Ok(response_text)
}

/// Build the tokio Command for running the agent process.
/// Uses cfg.command as the executable (defaults to ["claude"]).
/// When unix_user is set, wraps with sudo and strips SSH env vars.
pub fn build_command(cfg: &AgentConfig, args: &[String]) -> Command {
    let (bin, prefix_args) = split_command(&cfg.command);
    let mut cmd = match &cfg.unix_user {
        Some(user) => {
            let mut c = Command::new("sudo");
            c.args(["-u", user, "-H", "--"]);
            c.arg(bin);
            c.args(prefix_args);
            c.args(args);
            c.env_remove("SSH_AUTH_SOCK");
            c.env_remove("SSH_AGENT_PID");
            c
        }
        None => {
            let mut c = Command::new(bin);
            c.args(prefix_args);
            c.args(args);
            c
        }
    };
    cmd.current_dir(&cfg.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Split a command vec into (binary, prefix_args).
/// Falls back to "claude" if the vec is empty.
fn split_command(command: &[String]) -> (&str, &[String]) {
    match command {
        [] => ("claude", &[]),
        [bin] => (bin.as_str(), &[]),
        [bin, rest @ ..] => (bin.as_str(), rest),
    }
}

/// List all agents whose state files exist on disk.
pub async fn list() -> Result<Vec<AgentState>> {
    let dir = config::state_dir();
    let mut agents = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(state) = serde_yaml::from_str::<AgentState>(&content) {
                        agents.push(state);
                    }
                }
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
    if log.exists() {
        if let Err(e) = std::fs::remove_file(&log) {
            warn!(agent = %name, error = %e, "failed to remove log file");
        }
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
created_at: "2026-01-01T00:00:00Z"
"#;
        let state: AgentState = serde_yaml::from_str(yaml).unwrap();
        assert!(state.config.unix_user.is_none());
        assert_eq!(state.config.budget_usd, 50.0);
    }

    #[test]
    fn test_agent_config_with_unix_user() {
        let yaml = r#"
config:
  name: kira
  model: claude-opus-4-6
  system_prompt: "You are Kira."
  work_dir: /home/agent-kira
  max_turns: 50
  unix_user: agent-kira
  budget_usd: 25.0
pid: 0
session_id: "sess-abc"
total_turns: 10
total_cost: 1.23
created_at: "2026-01-01T00:00:00Z"
"#;
        let state: AgentState = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(state.config.unix_user.as_deref(), Some("agent-kira"));
        assert_eq!(state.config.budget_usd, 25.0);
        assert_eq!(state.session_id, "sess-abc");
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
        };
        let state = AgentState {
            config: cfg,
            pid: 0,
            session_id: "sid-123".to_string(),
            total_turns: 5,
            total_cost: 0.42,
            created_at: Utc::now().to_rfc3339(),
        };
        let yaml = serde_yaml::to_string(&state).unwrap();
        let restored: AgentState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(restored.config.unix_user.as_deref(), Some("agent1"));
        assert_eq!(restored.session_id, "sid-123");
        assert_eq!(restored.total_cost, 0.42);
        assert_eq!(restored.config.command, vec!["claude"]);
    }

    #[test]
    fn test_split_command_default() {
        let (bin, args) = split_command(&["claude".to_string()]);
        assert_eq!(bin, "claude");
        assert!(args.is_empty());
    }

    #[test]
    fn test_split_command_with_prefix_args() {
        let cmd = vec!["my-agent".to_string(), "--mode".to_string(), "chat".to_string()];
        let (bin, args) = split_command(&cmd);
        assert_eq!(bin, "my-agent");
        assert_eq!(args, &["--mode", "chat"]);
    }

    #[test]
    fn test_split_command_empty_falls_back() {
        let (bin, args) = split_command(&[]);
        assert_eq!(bin, "claude");
        assert!(args.is_empty());
    }

    #[test]
    fn test_agent_config_custom_command_serde() {
        let yaml = r#"
config:
  name: mybot
  model: claude-opus-4-6
  system_prompt: ""
  work_dir: /tmp
  max_turns: 100
  command:
    - my-agent-wrapper
    - --protocol
    - stream-json
pid: 0
session_id: ""
total_turns: 0
total_cost: 0.0
created_at: "2026-01-01T00:00:00Z"
"#;
        let state: AgentState = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(state.config.command, vec!["my-agent-wrapper", "--protocol", "stream-json"]);
    }
}
