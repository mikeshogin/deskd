use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

use crate::config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub work_dir: String,
    pub max_turns: u32,
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

/// Create a new agent — spawns claude as a background process.
pub async fn create(cfg: &AgentConfig) -> Result<AgentState> {
    // Check if already exists
    if state_path(&cfg.name).exists() {
        bail!("Agent '{}' already exists. Remove it first.", cfg.name);
    }

    let state = AgentState {
        config: cfg.clone(),
        pid: 0, // no persistent process — we run claude per-task
        session_id: String::new(),
        total_turns: 0,
        total_cost: 0.0,
        created_at: Utc::now().to_rfc3339(),
    };

    save_state(&state)?;
    Ok(state)
}

/// Send a message to an agent — runs claude CLI and returns response.
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

    let output = Command::new("claude")
        .args(&args)
        .current_dir(&state.config.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to run claude CLI")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse stream-json output — last line with type=result has cost/session info
    let mut response_text = String::new();
    let mut new_session_id = String::new();
    let mut task_cost = 0.0;
    let mut task_turns = 0u32;

    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("assistant") => {
                    // Extract text from content blocks
                    if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
                        if let Some(blocks) = content.as_array() {
                            for block in blocks {
                                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str())
                                    {
                                        response_text.push_str(text);
                                    }
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
                    if let Some(turns) = v.get("num_turns").and_then(|t| t.as_u64()) {
                        task_turns = turns as u32;
                    }
                }
                _ => {}
            }
        }
    }

    // Update state
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

/// List all agents.
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

/// Remove an agent.
pub async fn remove(name: &str) -> Result<()> {
    let path = state_path(name);
    if !path.exists() {
        bail!("Agent '{}' not found", name);
    }

    // Remove state file
    std::fs::remove_file(&path)?;

    // Remove log file if exists
    let log = log_path(name);
    if log.exists() {
        std::fs::remove_file(&log).ok();
    }

    Ok(())
}
