use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where agent state files are stored.
pub fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("agents");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Where agent logs are stored.
pub fn log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn default_max_turns() -> u32 {
    100
}

fn default_budget_usd() -> f64 {
    50.0
}

/// Top-level workspace configuration loaded from workspace.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub bus: BusConfig,
    #[serde(default)]
    pub adapters: AdaptersConfig,
    #[serde(default)]
    pub agents: Vec<AgentDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusConfig {
    #[serde(default = "default_socket")]
    pub socket: String,
}

fn default_socket() -> String {
    "/tmp/deskd.sock".to_string()
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            socket: default_socket(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdaptersConfig {
    pub telegram: Option<TelegramConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub system_prompt: String,
    pub work_dir: String,
    /// Optional Linux user to run the agent process as.
    pub unix_user: Option<String>,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Budget cap in USD. Worker rejects tasks when this is exceeded.
    #[serde(default = "default_budget_usd")]
    pub budget_usd: f64,
    /// Command to run as the agent process. Defaults to ["claude"].
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    /// Persistent agents are auto-started on `deskd serve` and restarted on
    /// crash. Non-persistent agents are spawned on demand. Default: true.
    #[serde(default = "default_persistent")]
    pub persistent: bool,
    /// Socket path for this agent's own sub-bus.
    /// Sub-agents spawned by this agent connect here, not to the root bus.
    /// If None, auto-derived as `{bus_dir}/{name}.sock`.
    pub sub_bus_socket: Option<String>,
}

fn default_command() -> Vec<String> {
    vec!["claude".to_string()]
}

fn default_persistent() -> bool {
    true
}

impl AgentDef {
    /// Compute the sub-bus socket path for this agent.
    pub fn sub_bus_path(&self, root_bus: &str) -> String {
        if let Some(ref p) = self.sub_bus_socket {
            return p.clone();
        }
        let root = std::path::Path::new(root_bus);
        let dir = root.parent().unwrap_or(std::path::Path::new("/tmp"));
        let stem = root.file_stem().and_then(|s| s.to_str()).unwrap_or("deskd");
        dir.join(format!("{}-{}.sock", stem, self.name))
            .to_string_lossy()
            .into_owned()
    }
}

impl WorkspaceConfig {
    /// Load and parse a workspace config file, expanding ${ENV_VAR} references.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read workspace config: {}", path))?;
        let expanded = expand_env_vars(&raw);
        let cfg: WorkspaceConfig = serde_yaml::from_str(&expanded)
            .context("failed to parse workspace config")?;
        Ok(cfg)
    }
}

/// Replace `${VAR}` and `$VAR` occurrences with their environment variable values.
/// Unknown variables are left as-is.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                // ${VAR} form
                chars.next(); // consume '{'
                let var: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Ok(val) = std::env::var(&var) {
                    result.push_str(&val);
                } else {
                    result.push_str(&format!("${{{}}}", var));
                }
            } else if chars.peek().map(|c| c.is_alphanumeric() || *c == '_').unwrap_or(false) {
                // $VAR form
                let mut var = String::new();
                while chars.peek().map(|c| c.is_alphanumeric() || *c == '_').unwrap_or(false) {
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
        std::env::set_var("TEST_TOKEN_DESKD", "abc123");
        let result = expand_env_vars("token: ${TEST_TOKEN_DESKD}");
        assert_eq!(result, "token: abc123");
    }

    #[test]
    fn test_expand_env_vars_dollar() {
        std::env::set_var("TEST_VAR_DESKD", "hello");
        let result = expand_env_vars("val: $TEST_VAR_DESKD end");
        assert_eq!(result, "val: hello end");
    }

    #[test]
    fn test_expand_env_vars_unknown_left_as_is() {
        let result = expand_env_vars("val: ${DEFINITELY_NOT_SET_XYZ123}");
        assert_eq!(result, "val: ${DEFINITELY_NOT_SET_XYZ123}");
    }

    #[test]
    fn test_workspace_config_defaults() {
        let yaml = r#"
agents:
  - name: kira
    model: claude-opus-4-6
    work_dir: /home/agent-kira
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].max_turns, 100);
        assert_eq!(cfg.agents[0].budget_usd, 50.0);
        assert!(cfg.agents[0].unix_user.is_none());
        assert_eq!(cfg.bus.socket, "/tmp/deskd.sock");
    }

    #[test]
    fn test_workspace_config_unix_user() {
        let yaml = r#"
agents:
  - name: kira
    model: claude-opus-4-6
    work_dir: /home/agent-kira
    unix_user: agent-kira
    budget_usd: 25.0
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].unix_user.as_deref(), Some("agent-kira"));
        assert_eq!(cfg.agents[0].budget_usd, 25.0);
    }

    #[test]
    fn test_agent_def_persistent_default() {
        let yaml = r#"
agents:
  - name: kira
    model: claude-opus-4-6
    work_dir: /tmp
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.agents[0].persistent, "persistent should default to true");
        assert!(cfg.agents[0].sub_bus_socket.is_none());
    }

    #[test]
    fn test_agent_def_non_persistent() {
        let yaml = r#"
agents:
  - name: worker
    model: claude-opus-4-6
    work_dir: /tmp
    persistent: false
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.agents[0].persistent);
    }

    #[test]
    fn test_sub_bus_path_auto_derived() {
        let def = AgentDef {
            name: "kira".to_string(),
            model: "m".to_string(),
            system_prompt: String::new(),
            work_dir: "/tmp".to_string(),
            unix_user: None,
            max_turns: 100,
            budget_usd: 50.0,
            command: vec!["claude".to_string()],
            persistent: true,
            sub_bus_socket: None,
        };
        assert_eq!(def.sub_bus_path("/run/deskd/root.sock"), "/run/deskd/root-kira.sock");
        assert_eq!(def.sub_bus_path("/tmp/deskd.sock"), "/tmp/deskd-kira.sock");
    }

    #[test]
    fn test_sub_bus_path_explicit() {
        let def = AgentDef {
            name: "kira".to_string(),
            model: "m".to_string(),
            system_prompt: String::new(),
            work_dir: "/tmp".to_string(),
            unix_user: None,
            max_turns: 100,
            budget_usd: 50.0,
            command: vec!["claude".to_string()],
            persistent: true,
            sub_bus_socket: Some("/custom/kira.sock".to_string()),
        };
        assert_eq!(def.sub_bus_path("/run/deskd/root.sock"), "/custom/kira.sock");
    }
}
