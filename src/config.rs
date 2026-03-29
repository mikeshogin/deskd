use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Where agent state files are stored (relative to $HOME).
pub fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("agents");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Where agent logs are stored (relative to $HOME).
pub fn log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Where one-shot reminder JSON files are stored.
pub fn reminders_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("reminders");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// A one-shot reminder that fires at a specific time and posts a message to the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemindDef {
    /// ISO 8601 timestamp at which to fire.
    pub at: String,
    /// Bus target (e.g. `agent:kira`).
    pub target: String,
    /// Payload text to post.
    pub message: String,
}

/// Derive the bus socket path for an agent from its work directory.
/// Convention: {work_dir}/.deskd/bus.sock
pub fn agent_bus_socket(work_dir: &str) -> String {
    PathBuf::from(work_dir)
        .join(".deskd")
        .join("bus.sock")
        .to_string_lossy()
        .into_owned()
}

fn default_max_turns() -> u32 {
    100
}

fn default_budget_usd() -> f64 {
    50.0
}

// ─── Root workspace.yaml ─────────────────────────────────────────────────────

/// Top-level workspace config (workspace.yaml).
/// Managed by root or the admin user. Defines top-level agents, their unix
/// users, Telegram bots, and the path to each agent's own deskd.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub agents: Vec<AgentDef>,
}

/// Telegram bot adapter config. Defined per-agent — each agent has its own bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather. Typically set via ${TELEGRAM_BOT_TOKEN}.
    pub token: String,
}

/// Discord bot adapter config. Defined per-agent in workspace.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Bot token from Discord Developer Portal. Typically set via ${DISCORD_BOT_TOKEN}.
    pub token: String,
}

/// Discord channel routing config in the per-user deskd.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordRoutesConfig {
    pub routes: Vec<DiscordRoute>,
}

/// A single Discord channel route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordRoute {
    /// Discord channel ID (u64).
    pub channel_id: u64,
    /// Human-readable name for this channel, shown to the agent as context.
    pub name: Option<String>,
}

/// Container runtime config for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    /// OCI image to use (e.g. "claude-code-local:official").
    pub image: String,
    /// Host paths to bind-mount. Format: "host_path" or "host_path:container_path"
    /// or "host_path:container_path:ro" for read-only.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Docker volumes. Format: "volume_name:container_path".
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Environment variables to set inside the container.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Container runtime binary. Defaults to "docker".
    #[serde(default = "default_container_runtime")]
    pub runtime: String,
}

fn default_container_runtime() -> String {
    "docker".to_string()
}

/// Definition of a top-level agent in workspace.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    /// Linux user to run the agent as. Required for isolation.
    pub unix_user: Option<String>,
    /// Agent's working directory (also determines bus socket path).
    pub work_dir: String,
    /// Path to the agent's own deskd.yaml. Defaults to {work_dir}/deskd.yaml.
    pub config: Option<String>,
    /// Telegram bot for this agent. When set, a Telegram adapter is started
    /// on the agent's bus when deskd serves this workspace.
    pub telegram: Option<TelegramConfig>,
    /// Discord bot for this agent. When set, a Discord adapter is started
    /// on the agent's bus when deskd serves this workspace.
    pub discord: Option<DiscordConfig>,
    /// Claude model override. Default is set in the agent's deskd.yaml.
    #[serde(default)]
    pub model: Option<String>,
    /// Command to run as the agent process. Defaults to ["claude"].
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    /// Budget cap in USD. Worker rejects tasks when exceeded.
    #[serde(default = "default_budget_usd")]
    pub budget_usd: f64,
    /// Container config. When set, the agent process runs inside a container.
    #[serde(default)]
    pub container: Option<ContainerConfig>,
}

impl AgentDef {
    /// Derive the path to the agent's deskd.yaml config file.
    pub fn config_path(&self) -> String {
        self.config.clone().unwrap_or_else(|| {
            PathBuf::from(&self.work_dir)
                .join("deskd.yaml")
                .to_string_lossy()
                .into_owned()
        })
    }

    /// Derive the agent's bus socket path.
    pub fn bus_socket(&self) -> String {
        agent_bus_socket(&self.work_dir)
    }
}

fn default_command() -> Vec<String> {
    vec!["claude".to_string()]
}

impl WorkspaceConfig {
    /// Load and parse a workspace config file, expanding ${ENV_VAR} references.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read workspace config: {}", path))?;
        let expanded = expand_env_vars(&raw);
        let cfg: WorkspaceConfig =
            serde_yaml::from_str(&expanded).context("failed to parse workspace config")?;
        Ok(cfg)
    }
}

// ─── Per-user deskd.yaml ─────────────────────────────────────────────────────

/// Per-user agent config (deskd.yaml, lives in the agent's work_dir).
/// Defines the agent's own model, system prompt, sub-agents, channels,
/// Telegram routes, and schedules. Managed by the agent's unix user.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    /// Claude model for the main agent. Overridden by workspace.yaml `model` if set.
    #[serde(default = "default_model")]
    pub model: String,
    /// System prompt for the main agent.
    #[serde(default)]
    pub system_prompt: String,
    /// Max turns per task.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Named broadcast/task channels this agent participates in.
    #[serde(default)]
    pub channels: Vec<ChannelDef>,
    /// Sub-agents spawned and managed within this agent's bus scope.
    #[serde(default)]
    pub agents: Vec<SubAgentDef>,
    /// Telegram channel routing for this agent.
    pub telegram: Option<TelegramRoutesConfig>,
    /// Discord channel routing for this agent.
    pub discord: Option<DiscordRoutesConfig>,
    /// Scheduled actions (cron → bus messages).
    #[serde(default)]
    pub schedules: Vec<ScheduleDef>,
    /// MCP server config JSON string or file path, passed to claude via --mcp-config.
    /// Example: '{"mcpServers":{"deskd":{"command":"deskd","args":["mcp","--agent","kira"]}}}'
    #[serde(default)]
    pub mcp_config: Option<String>,
    /// State machine model definitions.
    #[serde(default)]
    pub models: Vec<ModelDef>,
}

fn default_model() -> String {
    "claude-sonnet-4-6".to_string()
}

/// A named channel for broadcast or task-queue communication.
/// The name becomes the bus target, e.g. `news:ecosystem` or `queue:reviews`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDef {
    pub name: String,
    pub description: String,
}

/// Session mode for an agent: persistent (default) or ephemeral.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// Continue existing session across tasks (uses --resume).
    #[default]
    Persistent,
    /// Start a fresh session for each task (no --resume).
    Ephemeral,
}

/// A sub-agent running within a parent agent's bus scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentDef {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub system_prompt: String,
    /// Bus targets this agent receives messages from.
    /// Supports glob patterns: `telegram.in:*`, `agent:researcher`.
    pub subscribe: Vec<String>,
    /// Optional allow-list of targets this agent can publish to.
    /// If None, publish to any target is allowed.
    pub publish: Option<Vec<String>>,
    /// Session mode: persistent (default) or ephemeral.
    /// Ephemeral agents start a fresh session for each task.
    #[serde(default)]
    pub session: SessionMode,
}

/// Telegram channel routing config in the per-user deskd.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramRoutesConfig {
    pub routes: Vec<TelegramRoute>,
}

/// A single Telegram chat route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramRoute {
    /// Telegram chat_id (positive for users/groups, negative for channels/supergroups).
    pub chat_id: i64,
    /// If true, only respond when the bot is @mentioned in this chat.
    #[serde(default)]
    pub mention_only: bool,
    /// Human-readable name for this chat, shown to the agent as context.
    pub name: Option<String>,
    /// Bus target override. When set, incoming messages from this chat are published
    /// to this target (e.g. "agent:collab") instead of the default "telegram.in:<chat_id>".
    #[serde(default)]
    pub route_to: Option<String>,
}

/// A scheduled action that fires on a cron expression and posts a message to the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleDef {
    /// Cron expression, e.g. `"0 9 * * *"` for 9 AM daily.
    pub cron: String,
    /// Bus target to post to.
    pub target: String,
    /// What action to take when the schedule fires.
    pub action: ScheduleAction,
    /// Action-specific configuration (e.g. repos list for github_poll).
    pub config: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    /// Poll GitHub repos for issues with a label, post new issues to target.
    GithubPoll,
    /// Post a static payload string to the target.
    Raw,
    /// Run an arbitrary shell command via `sh -c`.
    /// `config.command` — the shell command to execute.
    /// If the command produces stdout and `target` is non-empty, stdout is posted to the bus.
    Shell,
}

/// A state machine model definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub states: Vec<String>,
    pub initial: String,
    #[serde(default)]
    pub terminal: Vec<String>,
    pub transitions: Vec<TransitionDef>,
}

/// A transition between states in a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionDef {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub on: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(rename = "type", default)]
    pub step_type: Option<String>,
    #[serde(default)]
    pub notify: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub timeout_goto: Option<String>,
}

impl UserConfig {
    /// Load and parse a deskd.yaml file, expanding ${ENV_VAR} references.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read user config: {}", path))?;
        let expanded = expand_env_vars(&raw);
        let cfg: UserConfig =
            serde_yaml::from_str(&expanded).context("failed to parse user config")?;
        Ok(cfg)
    }

    /// Build the MCP tool description for `send_message` based on available
    /// channels, sub-agents, and telegram routes.
    pub fn send_message_description(&self, agent_name: &str) -> String {
        let mut lines = vec![
            "Send a message to a target on the bus.".to_string(),
            String::new(),
            "Available targets:".to_string(),
        ];

        // Sub-agents
        for a in &self.agents {
            lines.push(format!(
                "  agent:{}  — {} ({}). {}",
                a.name,
                a.name,
                a.model,
                a.system_prompt.lines().next().unwrap_or("")
            ));
        }

        // Named channels
        for ch in &self.channels {
            lines.push(format!("  {}  — {}", ch.name, ch.description));
        }

        // Telegram outbound routes
        if let Some(tg) = &self.telegram {
            for route in &tg.routes {
                lines.push(format!(
                    "  telegram.out:{}  — Telegram chat {}",
                    route.chat_id, route.chat_id
                ));
            }
        }

        lines.push(String::new());
        lines.push(format!("You are agent '{}'.", agent_name));

        lines.join("\n")
    }
}

// ─── Env var expansion ────────────────────────────────────────────────────────

/// Replace `${VAR}` and `$VAR` occurrences with their environment variable values.
/// Unknown variables are left as-is.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let var: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Ok(val) = std::env::var(&var) {
                    result.push_str(&val);
                } else {
                    result.push_str(&format!("${{{}}}", var));
                }
            } else if chars
                .peek()
                .map(|c| c.is_alphanumeric() || *c == '_')
                .unwrap_or(false)
            {
                let mut var = String::new();
                while chars
                    .peek()
                    .map(|c| c.is_alphanumeric() || *c == '_')
                    .unwrap_or(false)
                {
                    var.push(chars.next().unwrap());
                }
                if let Ok(val) = std::env::var(&var) {
                    result.push_str(&val);
                } else {
                    result.push_str(&format!("${}", var));
                }
            } else {
                result.push(ch);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars_braces() {
        unsafe { std::env::set_var("TEST_TOKEN_DESKD", "abc123") };
        let result = expand_env_vars("token: ${TEST_TOKEN_DESKD}");
        assert_eq!(result, "token: abc123");
    }

    #[test]
    fn test_expand_env_vars_dollar() {
        unsafe { std::env::set_var("TEST_VAR_DESKD", "hello") };
        let result = expand_env_vars("val: $TEST_VAR_DESKD end");
        assert_eq!(result, "val: hello end");
    }

    #[test]
    fn test_expand_env_vars_unknown_left_as_is() {
        let result = expand_env_vars("val: ${DEFINITELY_NOT_SET_XYZ123}");
        assert_eq!(result, "val: ${DEFINITELY_NOT_SET_XYZ123}");
    }

    #[test]
    fn test_workspace_config_minimal() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
    unix_user: kira
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].name, "kira");
        assert_eq!(cfg.agents[0].unix_user.as_deref(), Some("kira"));
        assert!(cfg.agents[0].telegram.is_none());
        assert!(cfg.agents[0].config.is_none());
    }

    #[test]
    fn test_workspace_config_with_telegram() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
    unix_user: kira
    telegram:
      token: "bot-token-123"
  - name: dev
    work_dir: /home/dev
    unix_user: dev
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.agents[0].telegram.is_some());
        assert_eq!(
            cfg.agents[0].telegram.as_ref().unwrap().token,
            "bot-token-123"
        );
        assert!(cfg.agents[1].telegram.is_none());
    }

    #[test]
    fn test_agent_def_bus_socket() {
        let def = AgentDef {
            name: "kira".into(),
            unix_user: Some("kira".into()),
            work_dir: "/home/kira".into(),
            config: None,
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
        };
        assert_eq!(def.bus_socket(), "/home/kira/.deskd/bus.sock");
        assert_eq!(def.config_path(), "/home/kira/deskd.yaml");
    }

    #[test]
    fn test_agent_def_explicit_config_path() {
        let def = AgentDef {
            name: "kira".into(),
            unix_user: None,
            work_dir: "/home/kira".into(),
            config: Some("/etc/agents/kira.yaml".into()),
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
        };
        assert_eq!(def.config_path(), "/etc/agents/kira.yaml");
    }

    #[test]
    fn test_user_config_defaults() {
        let yaml = r#"
model: claude-opus-4-6
system_prompt: "You are Kira."
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.model, "claude-opus-4-6");
        assert_eq!(cfg.max_turns, 100);
        assert!(cfg.channels.is_empty());
        assert!(cfg.agents.is_empty());
        assert!(cfg.schedules.is_empty());
    }

    #[test]
    fn test_user_config_full() {
        let yaml = r#"
model: claude-opus-4-6
system_prompt: "You are Kira."

channels:
  - name: "news:ecosystem"
    description: "Ecosystem updates"
  - name: "queue:reviews"
    description: "PR review requests"

agents:
  - name: dev
    model: claude-sonnet-4-6
    system_prompt: "You implement code."
    subscribe:
      - "agent:dev"
    publish:
      - "agent:*"
      - "telegram.out:*"

  - name: researcher
    model: claude-haiku-4-5
    system_prompt: "You research topics."
    subscribe:
      - "agent:researcher"

telegram:
  routes:
    - chat_id: -1003733725513
    - chat_id: -1003754811357

schedules:
  - cron: "0 9 * * *"
    target: "telegram.out:-1003733725513"
    action: raw
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.channels.len(), 2);
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[0].subscribe, vec!["agent:dev"]);
        assert_eq!(cfg.agents[0].publish.as_ref().unwrap().len(), 2);
        assert!(cfg.agents[1].publish.is_none()); // allow all
        assert_eq!(cfg.telegram.unwrap().routes[0].chat_id, -1003733725513);
        assert_eq!(cfg.schedules.len(), 1);
    }

    #[test]
    fn test_sub_agent_session_mode() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"

agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe:
      - "agent:worker"
    session: ephemeral

  - name: researcher
    model: claude-sonnet-4-6
    system_prompt: "Researcher"
    subscribe:
      - "agent:researcher"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].session, SessionMode::Ephemeral);
        assert_eq!(cfg.agents[1].session, SessionMode::Persistent); // default
    }

    #[test]
    fn test_send_message_description() {
        let cfg = UserConfig {
            model: "claude-opus-4-6".into(),
            system_prompt: String::new(),
            max_turns: 100,
            channels: vec![ChannelDef {
                name: "news:ecosystem".into(),
                description: "Ecosystem updates".into(),
            }],
            agents: vec![SubAgentDef {
                name: "dev".into(),
                model: "claude-sonnet-4-6".into(),
                system_prompt: "Implements code changes.".into(),
                subscribe: vec!["agent:dev".into()],
                publish: None,
                session: SessionMode::default(),
            }],
            telegram: Some(TelegramRoutesConfig {
                routes: vec![TelegramRoute {
                    chat_id: -1003733725513,
                    mention_only: false,
                    name: None,
                    route_to: None,
                }],
            }),
            discord: None,
            schedules: vec![],
            mcp_config: None,
            models: vec![],
        };
        let desc = cfg.send_message_description("kira");
        assert!(desc.contains("agent:dev"));
        assert!(desc.contains("news:ecosystem"));
        assert!(desc.contains("telegram.out:-1003733725513"));
        assert!(desc.contains("You are agent 'kira'"));
    }

    #[test]
    fn test_workspace_config_with_container() {
        let yaml = r#"
agents:
  - name: dev
    work_dir: /home/dev
    container:
      image: claude-code-local:official
      mounts:
        - "~/.ssh:ro"
        - "~/.gitconfig:ro"
      volumes:
        - "claude-history:/commandhistory"
      env:
        GH_TOKEN: "my-token"
    command: [claude, --output-format, stream-json]
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let agent = &cfg.agents[0];
        assert!(agent.container.is_some());
        let c = agent.container.as_ref().unwrap();
        assert_eq!(c.image, "claude-code-local:official");
        assert_eq!(c.mounts.len(), 2);
        assert_eq!(c.volumes.len(), 1);
        assert_eq!(c.env.get("GH_TOKEN").unwrap(), "my-token");
        assert_eq!(c.runtime, "docker");
    }

    #[test]
    fn test_workspace_config_no_container() {
        let yaml = r#"
agents:
  - name: dev
    work_dir: /home/dev
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.agents[0].container.is_none());
    }

    #[test]
    fn test_telegram_route_with_route_to() {
        let yaml = r#"
telegram:
  routes:
    - chat_id: -1003733725513
      name: "personal"
    - chat_id: -1003754811357
      name: "collab"
      mention_only: true
      route_to: "agent:collab"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        let routes = cfg.telegram.unwrap().routes;
        assert_eq!(routes.len(), 2);
        assert!(routes[0].route_to.is_none());
        assert_eq!(routes[1].route_to.as_deref(), Some("agent:collab"));
        assert!(routes[1].mention_only);
    }

    #[test]
    fn test_agent_def_config_path_uses_agent_config_when_set() {
        // When an agent defines config_path in workspace.yaml, that path is used
        // for loading schedules and other agent-level config.
        let def = AgentDef {
            name: "family".into(),
            unix_user: Some("family".into()),
            work_dir: "/home/family".into(),
            config: Some("/home/family/deskd.yaml".into()),
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
        };
        assert_eq!(def.config_path(), "/home/family/deskd.yaml");
    }

    #[test]
    fn test_agent_def_config_path_defaults_to_work_dir() {
        // When config is not set, config_path defaults to {work_dir}/deskd.yaml.
        let def = AgentDef {
            name: "family".into(),
            unix_user: Some("family".into()),
            work_dir: "/home/family".into(),
            config: None,
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
        };
        assert_eq!(def.config_path(), "/home/family/deskd.yaml");
    }

    #[test]
    fn test_user_config_with_schedules() {
        // Verify that schedules defined in an agent-level deskd.yaml are parsed
        // correctly, supporting all three action types.
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
      command: "echo receipts"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.schedules.len(), 3);
        assert_eq!(cfg.schedules[0].cron, "3 7 * * *");
        assert!(matches!(cfg.schedules[0].action, ScheduleAction::Raw));
        assert!(matches!(
            cfg.schedules[1].action,
            ScheduleAction::GithubPoll
        ));
        assert!(matches!(cfg.schedules[2].action, ScheduleAction::Shell));
    }
}
