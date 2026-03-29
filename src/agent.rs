use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, AgentRuntime, ContainerConfig, SessionMode, UserConfig};

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
    /// Container config. When set, the agent runs inside a container.
    #[serde(default)]
    pub container: Option<ContainerConfig>,
    /// Session mode: persistent (default) or ephemeral.
    #[serde(default)]
    pub session: SessionMode,
    /// Agent runtime protocol: claude (default) or acp.
    #[serde(default)]
    pub runtime: AgentRuntime,
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
        container: def.container.clone(),
        session: SessionMode::default(),
        runtime: def.runtime.clone(),
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
    send_inner(name, message, max_turns, bus_socket, None, None, None).await
}

/// Format a plain text message as a stream-json user turn line (with trailing newline).
fn format_user_message(text: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        }
    });
    let mut line = serde_json::to_string(&msg).expect("json serialization cannot fail");
    line.push('\n');
    line
}

async fn send_inner(
    name: &str,
    message: &str,
    max_turns: Option<u32>,
    bus_socket: Option<&str>,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    image: Option<(&str, &str)>,
    inject_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
) -> Result<String> {
    let mut state = load_state(name)?;

    let turns = max_turns.unwrap_or(state.config.max_turns);

    // Always use stream-json input so we can keep stdin open for mid-task injection.
    let mut args: Vec<String> = Vec::new();

    // Use --resume only when session is persistent and session_id exists.
    let use_resume =
        state.config.session == SessionMode::Persistent && !state.session_id.is_empty();

    if use_resume {
        args.push("--resume".to_string());
        args.push(state.session_id.clone());
    }

    if !state.config.system_prompt.is_empty() && !use_resume {
        args.push("--system-prompt".to_string());
        args.push(state.config.system_prompt.clone());
    }

    // Always use stream-json input — needed for both multimodal content and mid-task injection.
    args.push("--input-format=stream-json".to_string());

    // Inject --model from agent config (sourced from deskd.yaml) unless the command
    // array already contains --model (e.g. hardcoded in workspace.yaml).
    if !state.config.model.is_empty()
        && !state
            .config
            .command
            .iter()
            .any(|a| a == "--model" || a.starts_with("--model="))
    {
        args.push("--model".to_string());
        args.push(state.config.model.clone());
    }

    debug!(agent = %name, turns, model = %state.config.model, multimodal = image.is_some(), "spawning claude");

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
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().context("Failed to spawn claude CLI")?;

    // Internal channel: all lines that should be written to Claude's stdin flow through here.
    let (stdin_line_tx, mut stdin_line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Build and send the initial user message (text + optional image).
    let initial_msg = if let Some((b64_data, media_type)) = image {
        let msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": b64_data,
                        }
                    },
                    {"type": "text", "text": message}
                ]
            }
        });
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        line
    } else {
        format_user_message(message)
    };

    stdin_line_tx
        .send(initial_msg)
        .expect("channel is open at this point");

    // If inject_rx is provided, spawn a task to forward injected messages as new user turns.
    if let Some(mut rx) = inject_rx {
        let tx = stdin_line_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let line = format_user_message(&msg);
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }

    // Stdin writer task: reads lines from the internal channel and writes them to Claude's stdin.
    // When all senders are dropped (inject task exits + we drop stdin_line_tx below),
    // stdin_line_rx drains and the task ends, closing Claude's stdin.
    let mut child_stdin = child.stdin.take().expect("stdin is piped");
    tokio::spawn(async move {
        while let Some(line) = stdin_line_rx.recv().await {
            if child_stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        // stdin closes here when all senders are dropped
    });

    // Drop our copy of the sender so that when the inject forwarder also exits,
    // the writer task sees EOF and closes Claude's stdin naturally.
    drop(stdin_line_tx);

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

    // Only save session_id for persistent agents.
    if state.config.session == SessionMode::Persistent && !new_session_id.is_empty() {
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
                image,
                None, // inject_rx consumed; don't retry injection
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
    if let Some(ref container) = cfg.container {
        return build_container_command(cfg, container, args, extra_env);
    }

    let (bin, prefix) = split_command(&cfg.command);
    let mut cmd = match &cfg.unix_user {
        Some(user) => {
            let mut c = Command::new("sudo");
            c.args(["-u", user, "-H", "--", "env"]);
            for (k, v) in extra_env {
                c.arg(format!("{}={}", k, v));
            }
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
    if cfg.unix_user.is_none() {
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
    }
    cmd.current_dir(&cfg.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Build a command that runs the agent process inside a container.
///
/// Constructs: `<runtime> run --rm -i --name deskd-<agent>
///   -v <work_dir>:<work_dir> -w <work_dir>
///   -v <bus_socket_dir>:<bus_socket_dir>
///   [-v <mount>...] [-v <volume>...] [-e <key>=<val>...]
///   <image> <command> <args>`
fn build_container_command(
    cfg: &AgentConfig,
    container: &ContainerConfig,
    args: &[String],
    extra_env: &[(&str, &str)],
) -> Command {
    let runtime = &container.runtime;
    let mut cmd = Command::new(runtime);

    cmd.args(["run", "--rm", "-i"]);
    cmd.args(["--name", &format!("deskd-{}", cfg.name)]);

    // Mount the agent's work_dir so the process can access the repo.
    cmd.args(["-v", &format!("{}:{}", cfg.work_dir, cfg.work_dir)]);
    cmd.args(["-w", &cfg.work_dir]);

    // The bus socket is at {work_dir}/.deskd/bus.sock — already covered by work_dir mount.

    // User-defined mounts from container config.
    for mount in &container.mounts {
        let expanded = normalize_mount(&expand_tilde(mount));
        cmd.args(["-v", &expanded]);
    }

    // Docker volumes.
    for vol in &container.volumes {
        cmd.args(["-v", vol]);
    }

    // Environment variables from container config.
    for (k, v) in &container.env {
        cmd.args(["-e", &format!("{}={}", k, v)]);
    }

    // Extra env vars (DESKD_BUS_SOCKET, DESKD_AGENT_NAME, DESKD_AGENT_CONFIG).
    for (k, v) in extra_env {
        cmd.args(["-e", &format!("{}={}", k, v)]);
    }

    // If there's a config_path, mount it into the container.
    if let Some(ref config_path) = cfg.config_path {
        let cp = PathBuf::from(config_path);
        if let Some(parent) = cp.parent() {
            let parent_str = parent.to_string_lossy();
            // Only mount if not already under work_dir.
            if !parent_str.starts_with(&cfg.work_dir) {
                cmd.args(["-v", &format!("{}:{}", parent_str, parent_str)]);
            }
        }
    }

    // Image.
    cmd.arg(&container.image);

    // The actual command to run inside the container.
    let (bin, prefix) = split_command(&cfg.command);
    cmd.arg(bin);
    cmd.args(prefix);
    cmd.args(args);

    // Don't set current_dir on the host — the container uses -w.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Expand ~ to $HOME in mount paths.
/// Handles formats: "~/foo", "~/foo:ro", "~/foo:~/bar", "~/foo:~/bar:ro".
fn expand_tilde(path: &str) -> String {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return path.to_string(),
    };

    // Split into parts, handling the colon-separated mount syntax.
    // Possible formats: "src", "src:ro", "src:dst", "src:dst:ro"
    let parts: Vec<&str> = path.splitn(3, ':').collect();
    let expand = |s: &str| -> String {
        if s.starts_with("~/") || s == "~" {
            s.replacen('~', &home, 1)
        } else {
            s.to_string()
        }
    };

    match parts.len() {
        1 => expand(parts[0]),
        2 => format!("{}:{}", expand(parts[0]), expand(parts[1])),
        3 => format!("{}:{}:{}", expand(parts[0]), expand(parts[1]), parts[2]),
        _ => path.to_string(),
    }
}

/// Normalize a mount spec to always have src:dst[:opts].
/// "path" → "path:path"
/// "path:ro" → "path:path:ro"
/// "src:dst" → "src:dst" (unchanged)
/// "src:dst:ro" → "src:dst:ro" (unchanged)
fn normalize_mount(mount: &str) -> String {
    let parts: Vec<&str> = mount.splitn(3, ':').collect();
    match parts.len() {
        1 => {
            // "path" → "path:path"
            format!("{}:{}", parts[0], parts[0])
        }
        2 => {
            // Could be "src:dst" or "path:ro"/"path:rw"
            if parts[1] == "ro" || parts[1] == "rw" {
                format!("{}:{}:{}", parts[0], parts[0], parts[1])
            } else {
                mount.to_string()
            }
        }
        _ => mount.to_string(),
    }
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
        container: None,
        session: SessionMode::default(),
        runtime: AgentRuntime::default(),
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

// ─── Persistent agent process ─────────────────────────────────────────────────

/// External limits enforced during a task. All control lives outside Claude.
#[derive(Debug, Clone, Default)]
pub struct TaskLimits {
    /// Max assistant turns (tool-use loops) before killing the process.
    pub max_turns: Option<u32>,
    /// Max cumulative cost (USD) for this agent before killing.
    pub budget_usd: Option<f64>,
}

/// Result of a single Claude turn (task).
pub struct TurnResult {
    pub response_text: String,
    pub session_id: String,
    pub cost_usd: f64,
    pub num_turns: u32,
}

/// Events emitted by the stdout reader task.
enum StdoutEvent {
    /// A text block from an `assistant` message.
    TextBlock(String),
    /// The `result` event marking end of a turn.
    Result(TurnResult),
    /// Process exited (stdout closed).
    ProcessExited,
}

/// A long-lived Claude process that accepts multiple tasks via stdin.
///
/// Usage:
///   let process = AgentProcess::start(name, bus_socket).await?;
///   let result = process.send_task(message, progress_tx, None, None).await?;
///   // process stays alive for the next task
///   process.stop().await;
pub struct AgentProcess {
    /// Send lines to Claude's stdin.
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Receive stdout events (text blocks + result).
    event_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<StdoutEvent>>,
    /// Child process handle for shutdown.
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    /// Agent name.
    name: String,
    /// Last cumulative cost reported by Claude (for computing deltas).
    /// Claude's `total_cost_usd` is session-cumulative, not per-task.
    last_reported_cost: tokio::sync::Mutex<f64>,
    /// Last cumulative turns reported by Claude.
    last_reported_turns: tokio::sync::Mutex<u32>,
}

impl AgentProcess {
    /// Spawn a persistent Claude process for the named agent.
    pub async fn start(name: &str, bus_socket: &str) -> Result<Self> {
        let (stdin_tx, event_rx, child) = Self::spawn_process(name, bus_socket, false).await?;

        Ok(Self {
            stdin_tx,
            event_rx: tokio::sync::Mutex::new(event_rx),
            child: tokio::sync::Mutex::new(Some(child)),
            name: name.to_string(),
            last_reported_cost: tokio::sync::Mutex::new(0.0),
            last_reported_turns: tokio::sync::Mutex::new(0),
        })
    }

    /// Spawn a persistent Claude process with a fresh session (no --resume).
    pub async fn start_fresh(name: &str, bus_socket: &str) -> Result<Self> {
        let (stdin_tx, event_rx, child) = Self::spawn_process(name, bus_socket, true).await?;

        Ok(Self {
            stdin_tx,
            event_rx: tokio::sync::Mutex::new(event_rx),
            child: tokio::sync::Mutex::new(Some(child)),
            name: name.to_string(),
            last_reported_cost: tokio::sync::Mutex::new(0.0),
            last_reported_turns: tokio::sync::Mutex::new(0),
        })
    }

    /// Core spawn logic — builds args, starts child, wires stdin/stdout tasks.
    /// When  is true, skip --resume regardless of session mode/state.
    async fn spawn_process(
        name: &str,
        bus_socket: &str,
        fresh: bool,
    ) -> Result<(
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<StdoutEvent>,
        tokio::process::Child,
    )> {
        let state = load_state(name)?;

        let mut args: Vec<String> = Vec::new();

        // Use --resume only when session is persistent and fresh is not requested.
        let use_resume = !fresh
            && state.config.session == SessionMode::Persistent
            && !state.session_id.is_empty();

        if use_resume {
            args.push("--resume".to_string());
            args.push(state.session_id.clone());
        }

        if !state.config.system_prompt.is_empty() && !use_resume {
            args.push("--system-prompt".to_string());
            args.push(state.config.system_prompt.clone());
        }

        args.push("--input-format=stream-json".to_string());

        if !state.config.model.is_empty()
            && !state
                .config
                .command
                .iter()
                .any(|a| a == "--model" || a.starts_with("--model="))
        {
            args.push("--model".to_string());
            args.push(state.config.model.clone());
        }

        let bus_path = bus_socket.to_string();
        let config_path_str = state.config.config_path.clone().unwrap_or_default();
        let mut extra_env: Vec<(&str, &str)> =
            vec![("DESKD_AGENT_NAME", name), ("DESKD_BUS_SOCKET", &bus_path)];
        if !config_path_str.is_empty() {
            extra_env.push(("DESKD_AGENT_CONFIG", &config_path_str));
        }

        let mut cmd = build_command(&state.config, &args, &extra_env);
        cmd.stdin(Stdio::piped());
        let mut child = cmd
            .spawn()
            .context("Failed to spawn persistent claude process")?;

        info!(agent = %name, model = %state.config.model, "persistent process started");

        // Take stdin/stdout.
        let child_stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        // Drain stderr in background.
        let agent_name = name.to_string();
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = tokio::io::BufReader::new(stderr);
            let _ = reader.read_to_string(&mut buf).await;
            if !buf.is_empty() {
                warn!(agent = %agent_name, stderr = %buf.trim(), "persistent process stderr");
            }
        });

        // Stdin writer task.
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut writer = child_stdin;
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Stdout reader task — parses stream-json and sends events.
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<StdoutEvent>();
        let agent_name2 = name.to_string();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
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
                                        && let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                    {
                                        block_text.push_str(text);
                                    }
                                }
                            }
                            if !block_text.is_empty()
                                && event_tx.send(StdoutEvent::TextBlock(block_text)).is_err()
                            {
                                break;
                            }
                        }
                        Some("result") => {
                            let session_id = v
                                .get("session_id")
                                .and_then(|s| s.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let cost_usd = v
                                .get("total_cost_usd")
                                .and_then(|c| c.as_f64())
                                .unwrap_or(0.0);
                            let num_turns =
                                v.get("num_turns").and_then(|t| t.as_u64()).unwrap_or(0) as u32;
                            // response_text is accumulated by send_task, not here.
                            if event_tx
                                .send(StdoutEvent::Result(TurnResult {
                                    response_text: String::new(),
                                    session_id,
                                    cost_usd,
                                    num_turns,
                                }))
                                .is_err()
                            {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            // stdout closed — process exited.
            let _ = event_tx.send(StdoutEvent::ProcessExited);
            debug!(agent = %agent_name2, "persistent process stdout closed");
        });

        Ok((stdin_tx, event_rx, child))
    }

    /// Send a task to the persistent process and collect the response.
    ///
    /// Enforces `limits` in real-time: if max_turns or budget is exceeded
    /// mid-task, the process is killed immediately.
    pub async fn send_task(
        &self,
        message: &str,
        progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
        image: Option<(&str, &str)>,
        limits: &TaskLimits,
    ) -> Result<TurnResult> {
        // Build the user message.
        let user_msg = if let Some((b64_data, media_type)) = image {
            let msg = serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": b64_data,
                            }
                        },
                        {"type": "text", "text": message}
                    ]
                }
            });
            let mut line = serde_json::to_string(&msg)?;
            line.push('\n');
            line
        } else {
            format_user_message(message)
        };

        // Send to stdin.
        self.stdin_tx
            .send(user_msg)
            .map_err(|_| anyhow::anyhow!("persistent process stdin closed"))?;

        // Read events until we get a Result, enforcing limits on each event.
        let mut event_rx = self.event_rx.lock().await;
        let mut response_text = String::new();
        let mut assistant_turns = 0u32;

        loop {
            match event_rx.recv().await {
                Some(StdoutEvent::TextBlock(text)) => {
                    assistant_turns += 1;

                    // Check turn limit.
                    if let Some(max) = limits.max_turns
                        && assistant_turns > max
                    {
                        warn!(
                            agent = %self.name,
                            turns = assistant_turns,
                            max = max,
                            "turn limit exceeded mid-task, killing process"
                        );
                        self.kill().await;
                        bail!(
                            "task killed: exceeded {} turn limit ({} turns)",
                            max,
                            assistant_turns
                        );
                    }

                    if let Some(tx) = &progress_tx {
                        let _ = tx.send(text.clone());
                    }
                    response_text.push_str(&text);
                }
                Some(StdoutEvent::Result(mut result)) => {
                    // Drain any trailing TextBlock events that arrived after the
                    // result event but belong to this same turn. Without this,
                    // the next call to send_task() would see stale blocks from
                    // the previous turn, causing an off-by-one where turn N's
                    // output appears as part of turn N+1's response. (#102)
                    while let Ok(StdoutEvent::TextBlock(trailing)) = event_rx.try_recv() {
                        debug!(
                            agent = %self.name,
                            len = trailing.len(),
                            "drained trailing text block after result event"
                        );
                        if let Some(tx) = &progress_tx {
                            let _ = tx.send(trailing.clone());
                        }
                        response_text.push_str(&trailing);
                    }

                    result.response_text = response_text;

                    // Compute deltas: Claude's total_cost_usd and num_turns are
                    // session-cumulative, not per-task. We track the last reported
                    // values to compute the actual delta for this task.
                    let mut last_cost = self.last_reported_cost.lock().await;
                    let mut last_turns = self.last_reported_turns.lock().await;
                    let cost_delta = (result.cost_usd - *last_cost).max(0.0);
                    let turns_delta = result.num_turns.saturating_sub(*last_turns);
                    *last_cost = result.cost_usd;
                    *last_turns = result.num_turns;
                    drop(last_cost);
                    drop(last_turns);

                    // Update state file with deltas.
                    if let Ok(mut state) = load_state(&self.name) {
                        // Only save session_id for persistent agents.
                        if state.config.session == SessionMode::Persistent
                            && !result.session_id.is_empty()
                        {
                            state.session_id = result.session_id.clone();
                        }
                        state.total_cost += cost_delta;
                        state.total_turns += turns_delta;
                        let _ = save_state(&state);

                        // Check budget limit after updating cost.
                        if let Some(budget) = limits.budget_usd
                            && state.total_cost >= budget
                        {
                            warn!(
                                agent = %self.name,
                                cost = state.total_cost,
                                budget = budget,
                                "budget exceeded, killing process"
                            );
                            self.kill().await;
                        }
                    }

                    return Ok(result);
                }
                Some(StdoutEvent::ProcessExited) | None => {
                    bail!("persistent process exited mid-task");
                }
            }
        }
    }

    /// Inject a message into the running process as a new user turn.
    pub fn inject_message(&self, message: &str) -> Result<()> {
        let line = format_user_message(message);
        self.stdin_tx
            .send(line)
            .map_err(|_| anyhow::anyhow!("persistent process stdin closed"))
    }

    /// Kill the running process immediately (e.g. budget exceeded mid-task).
    /// The process can be restarted later with a fresh AgentProcess::start().
    pub async fn kill(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        warn!(agent = %self.name, "persistent process killed");
    }

    /// Gracefully stop the persistent process.
    pub async fn stop(&self) {
        // Dropping all senders closes stdin, which causes Claude to exit.
        // We can't drop self.stdin_tx (owned), but we can kill the child.
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        info!(agent = %self.name, "persistent process stopped");
    }
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
            container: None,
            session: SessionMode::default(),
            runtime: AgentRuntime::default(),
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

    #[test]
    fn test_format_user_message() {
        let line = format_user_message("hello world");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello world");
    }

    #[test]
    fn test_expand_tilde_simple() {
        let result = expand_tilde("/absolute/path");
        assert_eq!(result, "/absolute/path");
    }

    #[test]
    fn test_expand_tilde_home() {
        let home = std::env::var("HOME").unwrap();
        let result = expand_tilde("~/.ssh");
        assert_eq!(result, format!("{}/.ssh", home));
    }

    #[test]
    fn test_expand_tilde_with_ro() {
        let home = std::env::var("HOME").unwrap();
        // expand_tilde treats "ro" as a path segment; normalize_mount fixes it later.
        let result = expand_tilde("~/.ssh:ro");
        assert_eq!(result, format!("{}/.ssh:ro", home));
    }

    #[test]
    fn test_normalize_mount_path_only() {
        assert_eq!(normalize_mount("/foo"), "/foo:/foo");
    }

    #[test]
    fn test_normalize_mount_path_ro() {
        assert_eq!(normalize_mount("/foo:ro"), "/foo:/foo:ro");
    }

    #[test]
    fn test_normalize_mount_src_dst() {
        assert_eq!(normalize_mount("/foo:/bar"), "/foo:/bar");
    }

    #[test]
    fn test_normalize_mount_src_dst_ro() {
        assert_eq!(normalize_mount("/foo:/bar:ro"), "/foo:/bar:ro");
    }

    #[test]
    fn test_expand_tilde_src_dst() {
        let home = std::env::var("HOME").unwrap();
        let result = expand_tilde("~/.gitconfig:~/.gitconfig:ro");
        assert_eq!(
            result,
            format!("{}/.gitconfig:{}/.gitconfig:ro", home, home)
        );
    }

    #[test]
    fn test_build_container_command() {
        use std::collections::HashMap;
        let mut env = HashMap::new();
        env.insert("GH_TOKEN".to_string(), "test-token".to_string());

        let container = ContainerConfig {
            image: "claude-code-local:official".to_string(),
            mounts: vec!["/host/path:/container/path:ro".to_string()],
            volumes: vec!["my-vol:/data".to_string()],
            env,
            runtime: "docker".to_string(),
        };

        let cfg = AgentConfig {
            name: "test-agent".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            system_prompt: String::new(),
            work_dir: "/home/test".to_string(),
            max_turns: 100,
            unix_user: None,
            budget_usd: 50.0,
            command: vec![
                "claude".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
            ],
            config_path: Some("/home/test/deskd.yaml".to_string()),
            container: Some(container),
            session: SessionMode::default(),
            runtime: AgentRuntime::default(),
        };

        let extra_env = [("DESKD_BUS_SOCKET", "/home/test/.deskd/bus.sock")];
        let args = vec!["--resume".to_string(), "session-1".to_string()];

        let cmd = build_command(&cfg, &args, &extra_env);
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(program, "docker");

        let cmd_args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // Verify basic structure.
        assert!(cmd_args.contains(&"run".to_string()));
        assert!(cmd_args.contains(&"--rm".to_string()));
        assert!(cmd_args.contains(&"-i".to_string()));
        assert!(cmd_args.contains(&"deskd-test-agent".to_string()));
        assert!(cmd_args.contains(&"/home/test:/home/test".to_string()));
        assert!(cmd_args.contains(&"/home/test".to_string()));
        assert!(cmd_args.contains(&"claude-code-local:official".to_string()));
        assert!(cmd_args.contains(&"/host/path:/container/path:ro".to_string()));
        assert!(cmd_args.contains(&"my-vol:/data".to_string()));
        assert!(cmd_args.contains(&"GH_TOKEN=test-token".to_string()));
        assert!(cmd_args.contains(&"DESKD_BUS_SOCKET=/home/test/.deskd/bus.sock".to_string()));
        // The agent command + args should be at the end.
        assert!(cmd_args.contains(&"claude".to_string()));
        assert!(cmd_args.contains(&"--output-format".to_string()));
        assert!(cmd_args.contains(&"stream-json".to_string()));
        assert!(cmd_args.contains(&"--resume".to_string()));
        assert!(cmd_args.contains(&"session-1".to_string()));
    }

    #[test]
    fn test_build_command_no_container() {
        let cfg = AgentConfig {
            name: "test".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            system_prompt: String::new(),
            work_dir: "/tmp".to_string(),
            max_turns: 100,
            unix_user: None,
            budget_usd: 50.0,
            command: vec!["claude".to_string()],
            config_path: None,
            container: None,
            session: SessionMode::default(),
            runtime: AgentRuntime::default(),
        };
        let cmd = build_command(&cfg, &[], &[]);
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(program, "claude");
    }
}
