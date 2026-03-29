use serde::{Deserialize, Serialize};

/// Node kinds in the context graph
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NodeKind {
    /// Injected as-is into the session
    Static {
        role: String, // "system", "user", "assistant"
        content: String,
    },
    /// Executed at fork time, result injected
    Live {
        command: String, // shell command to execute
        #[serde(default)]
        args: Vec<String>,
        max_age_secs: Option<u64>, // cache result for N seconds
        inject_as: String,         // role to inject result as
        #[serde(skip)]
        cached_result: Option<CachedResult>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResult {
    pub content: String,
    pub fetched_at: String, // RFC3339
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,        // human-readable description
    pub tokens_estimate: u32, // approximate token count
}

/// The main branch — persistent context for an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainBranch {
    pub agent: String,
    pub budget_tokens: u32, // target size for main
    pub nodes: Vec<Node>,   // ordered: stable first, dynamic last
}

/// Configuration for context system
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextConfig {
    pub enabled: bool,
    pub main_budget_tokens: Option<u32>,       // default 10000
    pub compact_threshold_tokens: Option<u32>, // trigger compaction at this session size, default 80000
    pub main_path: Option<String>,             // path to main branch file
}

impl MainBranch {
    pub fn new(agent: &str, budget: u32) -> Self {
        Self {
            agent: agent.to_string(),
            budget_tokens: budget,
            nodes: Vec::new(),
        }
    }

    /// Load from YAML file
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }

    /// Save to YAML file
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_yaml::to_string(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Total estimated tokens across all nodes
    pub fn total_tokens(&self) -> u32 {
        self.nodes.iter().map(|n| n.tokens_estimate).sum()
    }

    /// Materialize: execute live nodes and produce message list for session injection
    pub async fn materialize(&mut self) -> anyhow::Result<Vec<MaterializedMessage>> {
        let mut messages = Vec::new();
        for node in &mut self.nodes {
            match &mut node.kind {
                NodeKind::Static { role, content } => {
                    messages.push(MaterializedMessage {
                        role: role.clone(),
                        content: content.clone(),
                    });
                }
                NodeKind::Live {
                    command,
                    args,
                    max_age_secs,
                    inject_as,
                    cached_result,
                } => {
                    // Check cache
                    let use_cache =
                        if let (Some(cached), Some(max_age)) = (&cached_result, max_age_secs) {
                            if let Ok(fetched) =
                                chrono::DateTime::parse_from_rfc3339(&cached.fetched_at)
                            {
                                let age = chrono::Utc::now().signed_duration_since(fetched);
                                age.num_seconds() < *max_age as i64
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                    let content = if use_cache {
                        cached_result.as_ref().unwrap().content.clone()
                    } else {
                        // Execute command
                        let output = tokio::process::Command::new(&*command)
                            .args(args.iter())
                            .output()
                            .await?;
                        let result = String::from_utf8_lossy(&output.stdout).to_string();
                        *cached_result = Some(CachedResult {
                            content: result.clone(),
                            fetched_at: chrono::Utc::now().to_rfc3339(),
                        });
                        result
                    };

                    if !content.trim().is_empty() {
                        messages.push(MaterializedMessage {
                            role: inject_as.clone(),
                            content: format!("[{}] {}", node.label, content.trim()),
                        });
                    }
                }
            }
        }
        Ok(messages)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedMessage {
    pub role: String,
    pub content: String,
}

/// Check if session should be compacted based on cumulative token usage
pub fn should_compact(total_tokens_used: u64, threshold: u64) -> bool {
    total_tokens_used >= threshold
}

/// Default path for the main branch file relative to an agent's work_dir.
pub fn default_main_path(work_dir: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(work_dir)
        .join(".deskd")
        .join("context")
        .join("main.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_main_branch_new() {
        let branch = MainBranch::new("kira", 10000);
        assert_eq!(branch.agent, "kira");
        assert_eq!(branch.budget_tokens, 10000);
        assert!(branch.nodes.is_empty());
    }

    #[test]
    fn test_total_tokens_empty() {
        let branch = MainBranch::new("kira", 10000);
        assert_eq!(branch.total_tokens(), 0);
    }

    #[test]
    fn test_total_tokens_with_nodes() {
        let mut branch = MainBranch::new("kira", 10000);
        branch.nodes.push(Node {
            id: "n1".into(),
            kind: NodeKind::Static {
                role: "system".into(),
                content: "You are Kira.".into(),
            },
            label: "System prompt".into(),
            tokens_estimate: 500,
        });
        branch.nodes.push(Node {
            id: "n2".into(),
            kind: NodeKind::Static {
                role: "user".into(),
                content: "Governance rules...".into(),
            },
            label: "Governance".into(),
            tokens_estimate: 1200,
        });
        assert_eq!(branch.total_tokens(), 1700);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("deskd_test_context_roundtrip");
        let path = dir.join("main.yaml");

        let mut branch = MainBranch::new("test-agent", 8000);
        branch.nodes.push(Node {
            id: "s1".into(),
            kind: NodeKind::Static {
                role: "system".into(),
                content: "Hello world".into(),
            },
            label: "Greeting".into(),
            tokens_estimate: 100,
        });
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["test".into()],
                max_age_secs: Some(300),
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Echo test".into(),
            tokens_estimate: 50,
        });

        branch.save(&path).expect("save failed");
        let loaded = MainBranch::load(&path).expect("load failed");

        assert_eq!(loaded.agent, "test-agent");
        assert_eq!(loaded.budget_tokens, 8000);
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.nodes[0].id, "s1");
        assert_eq!(loaded.nodes[0].label, "Greeting");
        assert_eq!(loaded.total_tokens(), 150);

        // Verify static node content
        match &loaded.nodes[0].kind {
            NodeKind::Static { role, content } => {
                assert_eq!(role, "system");
                assert_eq!(content, "Hello world");
            }
            _ => panic!("Expected Static node"),
        }

        // Verify live node content
        match &loaded.nodes[1].kind {
            NodeKind::Live {
                command,
                args,
                max_age_secs,
                inject_as,
                cached_result,
            } => {
                assert_eq!(command, "echo");
                assert_eq!(args, &["test"]);
                assert_eq!(*max_age_secs, Some(300));
                assert_eq!(inject_as, "user");
                // cached_result is #[serde(skip)] so should be None after load
                assert!(cached_result.is_none());
            }
            _ => panic!("Expected Live node"),
        }

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_materialize_static_nodes() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "s1".into(),
            kind: NodeKind::Static {
                role: "system".into(),
                content: "You are a test agent.".into(),
            },
            label: "System".into(),
            tokens_estimate: 100,
        });
        branch.nodes.push(Node {
            id: "s2".into(),
            kind: NodeKind::Static {
                role: "user".into(),
                content: "Some context".into(),
            },
            label: "Context".into(),
            tokens_estimate: 50,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "You are a test agent.");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "Some context");
    }

    #[tokio::test]
    async fn test_materialize_live_node_echo() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["hello from live node".into()],
                max_age_secs: None,
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Echo test".into(),
            tokens_estimate: 50,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "[Echo test] hello from live node");
    }

    #[tokio::test]
    async fn test_materialize_live_node_caches_result() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["cached".into()],
                max_age_secs: Some(3600), // 1 hour cache
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Cached echo".into(),
            tokens_estimate: 50,
        });

        // First materialize — executes command
        let messages = branch
            .materialize()
            .await
            .expect("first materialize failed");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "[Cached echo] cached");

        // Verify cache was populated
        match &branch.nodes[0].kind {
            NodeKind::Live { cached_result, .. } => {
                assert!(cached_result.is_some());
                let cached = cached_result.as_ref().unwrap();
                assert_eq!(cached.content, "cached\n");
            }
            _ => panic!("Expected Live node"),
        }

        // Second materialize — should use cache (max_age_secs=3600, just ran)
        let messages2 = branch
            .materialize()
            .await
            .expect("second materialize failed");
        assert_eq!(messages2.len(), 1);
        assert_eq!(messages2[0].content, "[Cached echo] cached");
    }

    #[tokio::test]
    async fn test_materialize_live_node_expired_cache() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["fresh".into()],
                max_age_secs: Some(60),
                inject_as: "user".into(),
                // Pre-populate with an expired cache entry
                cached_result: Some(CachedResult {
                    content: "stale data".into(),
                    fetched_at: "2020-01-01T00:00:00Z".into(), // long expired
                }),
            },
            label: "Expiry test".into(),
            tokens_estimate: 50,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert_eq!(messages.len(), 1);
        // Should have re-executed, getting "fresh" from echo
        assert_eq!(messages[0].content, "[Expiry test] fresh");
    }

    #[tokio::test]
    async fn test_materialize_empty_output_skipped() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "true".into(), // produces no output
                args: vec![],
                max_age_secs: None,
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Silent".into(),
            tokens_estimate: 10,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert!(messages.is_empty(), "Empty output should be skipped");
    }

    #[test]
    fn test_should_compact() {
        assert!(!should_compact(50000, 80000));
        assert!(should_compact(80000, 80000));
        assert!(should_compact(100000, 80000));
        assert!(!should_compact(0, 80000));
    }

    #[test]
    fn test_default_main_path() {
        let path = default_main_path("/home/kira");
        assert_eq!(path, PathBuf::from("/home/kira/.deskd/context/main.yaml"));
    }

    #[test]
    fn test_context_config_defaults() {
        let cfg = ContextConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.main_budget_tokens.is_none());
        assert!(cfg.compact_threshold_tokens.is_none());
        assert!(cfg.main_path.is_none());
    }

    #[test]
    fn test_context_config_serde() {
        let yaml = r#"
enabled: true
main_budget_tokens: 12000
compact_threshold_tokens: 90000
main_path: /home/kira/.deskd/context/main.yaml
"#;
        let cfg: ContextConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.main_budget_tokens, Some(12000));
        assert_eq!(cfg.compact_threshold_tokens, Some(90000));
        assert_eq!(
            cfg.main_path.as_deref(),
            Some("/home/kira/.deskd/context/main.yaml")
        );
    }
}
