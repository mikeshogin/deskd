//! session2graph — Convert Claude Code session JSONL files into archlint-compatible
//! architecture.yaml format, or compute metrics from the session graph.
//!
//! Usage:
//!   session2graph <input.jsonl>                    # YAML graph to stdout
//!   session2graph <input.jsonl> --output out.yaml  # YAML graph to file
//!   session2graph <input.jsonl> --metrics          # JSON metrics to stdout

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

// ── CLI ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "session2graph",
    about = "Convert Claude Code session JSONL to architecture.yaml"
)]
struct Cli {
    /// Path to the session JSONL file
    input: PathBuf,

    /// Output file path (default: stdout)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output JSON metrics report instead of YAML graph
    #[arg(long)]
    metrics: bool,
}

// ── Input types (JSONL) ──────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SessionLine {
    #[serde(default)]
    #[allow(dead_code)]
    r#type: String,
    #[serde(default)]
    #[allow(dead_code)]
    uuid: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    parent_uuid: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize, Debug)]
struct Message {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: MessageContent,
}

#[derive(Deserialize, Debug, Default)]
#[serde(untagged)]
enum MessageContent {
    #[default]
    Empty,
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: serde_json::Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    #[serde(other)]
    Unknown,
}

// ── Output types (YAML) ─────────────────────────────────────────────

#[derive(Serialize)]
struct ArchGraph {
    components: Vec<Component>,
    links: Vec<Link>,
}

#[derive(Serialize)]
struct Component {
    id: String,
    title: String,
    entity: String,
}

#[derive(Serialize)]
struct Link {
    from: String,
    to: String,
    r#type: String,
}

// ── Conversion ───────────────────────────────────────────────────────

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

fn tool_use_title(name: &str, input: &serde_json::Value) -> String {
    let args = match input {
        serde_json::Value::Object(map) => {
            let keys: Vec<&str> = map.keys().take(2).map(|k| k.as_str()).collect();
            let parts: Vec<String> = keys
                .iter()
                .filter_map(|k| {
                    map.get(*k).map(|v| match v {
                        serde_json::Value::String(s) => truncate(s, 30),
                        _ => truncate(&v.to_string(), 30),
                    })
                })
                .collect();
            parts.join(", ")
        }
        _ => String::new(),
    };
    format!("tool_use: {name}({args})")
}

fn tool_result_title(content: &serde_json::Value) -> String {
    let len = match content {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(arr) => {
            // Sum up text content blocks
            arr.iter()
                .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                .map(|s| s.len())
                .sum()
        }
        _ => content.to_string().len(),
    };
    format!("tool_result: [{len} chars]")
}

struct GraphBuilder {
    components: Vec<Component>,
    links: Vec<Link>,
    counter: usize,
    // Map tool_use id -> component id for linking results back
    tool_use_ids: HashMap<String, String>,
    // Track last component id per message for chaining
    last_assistant_id: Option<String>,
    last_user_id: Option<String>,
}

impl GraphBuilder {
    fn new() -> Self {
        Self {
            components: Vec::new(),
            links: Vec::new(),
            counter: 0,
            tool_use_ids: HashMap::new(),
            last_assistant_id: None,
            last_user_id: None,
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}_{:03}", self.counter)
    }

    fn process_line(&mut self, line: &SessionLine) {
        let message = match &line.message {
            Some(m) => m,
            None => return,
        };

        let owned_blocks;
        let blocks: &[ContentBlock] = match &message.content {
            MessageContent::Blocks(b) => b,
            MessageContent::Text(s) => {
                owned_blocks = vec![ContentBlock::Text { text: s.clone() }];
                &owned_blocks
            }
            MessageContent::Empty => return,
        };

        // Track whether we produced any components from this message to link via parentUuid
        let mut first_component_of_message: Option<String> = None;

        for block in blocks {
            match block {
                ContentBlock::Text { text } if message.role == "user" => {
                    let id = self.next_id("msg");
                    let title = format!("user: {}", truncate(text, 60));

                    // Link from previous assistant to this user (conversation flow)
                    if let Some(ref prev) = self.last_assistant_id {
                        self.links.push(Link {
                            from: prev.clone(),
                            to: id.clone(),
                            r#type: "response".into(),
                        });
                    }

                    self.components.push(Component {
                        id: id.clone(),
                        title,
                        entity: "user_message".into(),
                    });
                    self.last_user_id = Some(id.clone());
                    if first_component_of_message.is_none() {
                        first_component_of_message = Some(id);
                    }
                }
                ContentBlock::Text { text } if message.role == "assistant" => {
                    let id = self.next_id("msg");
                    let title = format!("assistant: {}", truncate(text, 60));

                    // Link user -> assistant
                    if let Some(ref prev) = self.last_user_id {
                        self.links.push(Link {
                            from: prev.clone(),
                            to: id.clone(),
                            r#type: "response".into(),
                        });
                    }

                    self.components.push(Component {
                        id: id.clone(),
                        title,
                        entity: "assistant_message".into(),
                    });
                    self.last_assistant_id = Some(id.clone());
                    if first_component_of_message.is_none() {
                        first_component_of_message = Some(id);
                    }
                }
                ContentBlock::ToolUse {
                    id: tid,
                    name,
                    input,
                } => {
                    let comp_id = self.next_id("tool");
                    let title = tool_use_title(name, input);

                    // Link assistant -> tool_use
                    if let Some(ref prev) = self.last_assistant_id {
                        self.links.push(Link {
                            from: prev.clone(),
                            to: comp_id.clone(),
                            r#type: "invocation".into(),
                        });
                    }

                    self.tool_use_ids.insert(tid.clone(), comp_id.clone());
                    self.components.push(Component {
                        id: comp_id.clone(),
                        title,
                        entity: "tool_use".into(),
                    });
                    if first_component_of_message.is_none() {
                        first_component_of_message = Some(comp_id);
                    }
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => {
                    let comp_id = format!("{}r", self.next_id("tool"));
                    let title = tool_result_title(content);

                    // Link tool_use -> tool_result
                    if let Some(from_id) = self.tool_use_ids.get(tool_use_id) {
                        self.links.push(Link {
                            from: from_id.clone(),
                            to: comp_id.clone(),
                            r#type: "result".into(),
                        });
                    }

                    self.components.push(Component {
                        id: comp_id.clone(),
                        title,
                        entity: "tool_result".into(),
                    });

                    // tool_result feeds into next assistant turn — track it
                    // so the next assistant text can link from it
                    self.last_user_id = Some(comp_id.clone());
                    if first_component_of_message.is_none() {
                        first_component_of_message = Some(comp_id);
                    }
                }
                ContentBlock::Thinking { thinking } => {
                    let id = self.next_id("msg");
                    let title = format!("thinking: {}", truncate(thinking, 60));

                    if let Some(ref prev) = self.last_user_id {
                        self.links.push(Link {
                            from: prev.clone(),
                            to: id.clone(),
                            r#type: "response".into(),
                        });
                    }

                    self.components.push(Component {
                        id: id.clone(),
                        title,
                        entity: "thinking".into(),
                    });
                    self.last_assistant_id = Some(id.clone());
                    if first_component_of_message.is_none() {
                        first_component_of_message = Some(id);
                    }
                }
                _ => {}
            }
        }
    }

    fn build(self) -> ArchGraph {
        ArchGraph {
            components: self.components,
            links: self.links,
        }
    }
}

fn convert(input: &str) -> Result<ArchGraph> {
    let mut builder = GraphBuilder::new();

    for (i, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionLine>(line) {
            Ok(session_line) => builder.process_line(&session_line),
            Err(e) => {
                eprintln!("warning: skipping line {}: {e}", i + 1);
            }
        }
    }

    Ok(builder.build())
}

// ── Metrics ─────────────────────────────────────────────────────────

#[derive(Serialize, Debug)]
struct MetricsReport {
    summary: MetricsSummary,
    tool_usage: Vec<ToolUsageEntry>,
    fan_out: FanOut,
    patterns: Vec<PatternEntry>,
    session_stats: SessionStats,
}

#[derive(Serialize, Debug)]
struct MetricsSummary {
    total_nodes: usize,
    total_edges: usize,
    nodes_by_type: HashMap<String, usize>,
    edges_by_type: HashMap<String, usize>,
}

#[derive(Serialize, Debug)]
struct ToolUsageEntry {
    name: String,
    count: usize,
}

#[derive(Serialize, Debug)]
struct FanOut {
    max: usize,
    max_node: String,
    average: f64,
}

#[derive(Serialize, Debug)]
struct PatternEntry {
    from: String,
    to: String,
    count: usize,
}

#[derive(Serialize, Debug)]
struct SessionStats {
    user_messages: usize,
    assistant_messages: usize,
    tool_calls: usize,
    thinking_blocks: usize,
    tool_to_text_ratio: f64,
}

fn compute_metrics(graph: &ArchGraph) -> MetricsReport {
    // Summary: nodes by type, edges by type
    let mut nodes_by_type: HashMap<String, usize> = HashMap::new();
    for comp in &graph.components {
        *nodes_by_type.entry(comp.entity.clone()).or_default() += 1;
    }

    let mut edges_by_type: HashMap<String, usize> = HashMap::new();
    for link in &graph.links {
        *edges_by_type.entry(link.r#type.clone()).or_default() += 1;
    }

    let summary = MetricsSummary {
        total_nodes: graph.components.len(),
        total_edges: graph.links.len(),
        nodes_by_type,
        edges_by_type,
    };

    // Tool usage: count per tool name from tool_use titles
    let mut tool_counts: HashMap<String, usize> = HashMap::new();
    for comp in &graph.components {
        if comp.entity == "tool_use" {
            // Title format: "tool_use: Name(args)"
            if let Some(name) = comp.title.strip_prefix("tool_use: ") {
                let name = name.find('(').map(|i| &name[..i]).unwrap_or(name);
                *tool_counts.entry(name.to_string()).or_default() += 1;
            }
        }
    }
    let mut tool_usage: Vec<ToolUsageEntry> = tool_counts
        .into_iter()
        .map(|(name, count)| ToolUsageEntry { name, count })
        .collect();
    tool_usage.sort_by(|a, b| b.count.cmp(&a.count));

    // Fan-out: outgoing edges per node
    let mut outgoing: HashMap<String, usize> = HashMap::new();
    for link in &graph.links {
        *outgoing.entry(link.from.clone()).or_default() += 1;
    }
    let max_fan_out = outgoing.iter().max_by_key(|(_, v)| *v);
    let (max_node, max) = match max_fan_out {
        Some((node, count)) => (node.clone(), *count),
        None => (String::new(), 0),
    };
    let total_nodes_with_edges = outgoing.len();
    let sum_outgoing: usize = outgoing.values().sum();
    let average = if total_nodes_with_edges > 0 {
        // Round to 1 decimal
        ((sum_outgoing as f64 / total_nodes_with_edges as f64) * 10.0).round() / 10.0
    } else {
        0.0
    };

    let fan_out = FanOut {
        max,
        max_node,
        average,
    };

    // Patterns: tool_use bigrams that repeat > 3 times
    // Collect sequential tool_use names in order
    let tool_names: Vec<String> = graph
        .components
        .iter()
        .filter(|c| c.entity == "tool_use")
        .filter_map(|c| {
            c.title.strip_prefix("tool_use: ").map(|name| {
                name.find('(')
                    .map(|i| name[..i].to_string())
                    .unwrap_or_else(|| name.to_string())
            })
        })
        .collect();

    let mut bigram_counts: HashMap<(String, String), usize> = HashMap::new();
    for pair in tool_names.windows(2) {
        let key = (pair[0].clone(), pair[1].clone());
        *bigram_counts.entry(key).or_default() += 1;
    }

    let mut patterns: Vec<PatternEntry> = bigram_counts
        .into_iter()
        .filter(|(_, count)| *count > 3)
        .map(|((from, to), count)| PatternEntry { from, to, count })
        .collect();
    patterns.sort_by(|a, b| b.count.cmp(&a.count));

    // Session stats
    let user_messages = graph
        .components
        .iter()
        .filter(|c| c.entity == "user_message")
        .count();
    let assistant_messages = graph
        .components
        .iter()
        .filter(|c| c.entity == "assistant_message")
        .count();
    let tool_calls = graph
        .components
        .iter()
        .filter(|c| c.entity == "tool_use")
        .count();
    let thinking_blocks = graph
        .components
        .iter()
        .filter(|c| c.entity == "thinking")
        .count();
    let text_count = user_messages + assistant_messages;
    let tool_to_text_ratio = if text_count > 0 {
        ((tool_calls as f64 / text_count as f64) * 100.0).round() / 100.0
    } else {
        0.0
    };

    let session_stats = SessionStats {
        user_messages,
        assistant_messages,
        tool_calls,
        thinking_blocks,
        tool_to_text_ratio,
    };

    MetricsReport {
        summary,
        tool_usage,
        fan_out,
        patterns,
        session_stats,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let input = fs::read_to_string(&cli.input)
        .with_context(|| format!("failed to read {}", cli.input.display()))?;

    let graph = convert(&input)?;

    if cli.metrics {
        let report = compute_metrics(&graph);
        let json =
            serde_json::to_string_pretty(&report).context("failed to serialize metrics JSON")?;
        io::stdout().write_all(json.as_bytes())?;
        io::stdout().write_all(b"\n")?;
    } else {
        let yaml = serde_yaml::to_string(&graph).context("failed to serialize YAML")?;
        match cli.output {
            Some(path) => {
                fs::write(&path, &yaml)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            None => {
                io::stdout().write_all(yaml.as_bytes())?;
            }
        }
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":[{"type":"text","text":"fix the login bug in auth.rs"}]},"timestamp":"2025-01-01T00:00:00Z"}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":"I'll look at auth.rs to find the login bug."},{"type":"tool_use","id":"tu1","name":"Read","input":{"file_path":"src/auth.rs"}}]},"timestamp":"2025-01-01T00:00:01Z"}
{"type":"user","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"fn login() { ... }"}]},"timestamp":"2025-01-01T00:00:02Z"}
{"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"role":"assistant","content":[{"type":"text","text":"Found the bug — the password check is missing. Let me fix it."},{"type":"tool_use","id":"tu2","name":"Edit","input":{"file_path":"src/auth.rs","old_string":"fn login()","new_string":"fn login_fixed()"}}]},"timestamp":"2025-01-01T00:00:03Z"}
"#;

    #[test]
    fn test_basic_conversion() {
        let graph = convert(FIXTURE).unwrap();

        // Should have: user_msg, assistant_msg, tool_use(Read), tool_result, assistant_msg, tool_use(Edit)
        assert_eq!(graph.components.len(), 6);

        // Check entity types
        assert_eq!(graph.components[0].entity, "user_message");
        assert_eq!(graph.components[1].entity, "assistant_message");
        assert_eq!(graph.components[2].entity, "tool_use");
        assert_eq!(graph.components[3].entity, "tool_result");
        assert_eq!(graph.components[4].entity, "assistant_message");
        assert_eq!(graph.components[5].entity, "tool_use");

        // Check titles
        assert!(
            graph.components[0]
                .title
                .starts_with("user: fix the login bug")
        );
        assert!(graph.components[2].title.contains("Read"));
        assert!(graph.components[3].title.contains("chars]"));
        assert!(graph.components[5].title.contains("Edit"));

        // Check links exist
        assert!(!graph.links.is_empty());

        // user -> assistant response link
        let first_link = &graph.links[0];
        assert_eq!(first_link.r#type, "response");

        // assistant -> tool invocation link
        let invocation_links: Vec<_> = graph
            .links
            .iter()
            .filter(|l| l.r#type == "invocation")
            .collect();
        assert_eq!(invocation_links.len(), 2); // Read and Edit

        // tool_use -> tool_result link
        let result_links: Vec<_> = graph
            .links
            .iter()
            .filter(|l| l.r#type == "result")
            .collect();
        assert_eq!(result_links.len(), 1);
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 60), "short");
        let long = "a".repeat(100);
        let result = truncate(&long, 60);
        assert!(result.len() <= 64); // 60 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_tool_result_title() {
        let content = serde_json::Value::String("hello world".into());
        assert_eq!(tool_result_title(&content), "tool_result: [11 chars]");
    }

    #[test]
    fn test_empty_input() {
        let graph = convert("").unwrap();
        assert!(graph.components.is_empty());
        assert!(graph.links.is_empty());
    }

    #[test]
    fn test_yaml_output_format() {
        let graph = convert(FIXTURE).unwrap();
        let yaml = serde_yaml::to_string(&graph).unwrap();

        // Should contain expected keys
        assert!(yaml.contains("components:"));
        assert!(yaml.contains("links:"));
        assert!(yaml.contains("entity: user_message"));
        assert!(yaml.contains("entity: tool_use"));
        assert!(yaml.contains("type: response"));
        assert!(yaml.contains("type: invocation"));
        assert!(yaml.contains("type: result"));
    }

    #[test]
    fn test_thinking_block() {
        let input = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me think about this carefully..."},{"type":"text","text":"Here is my answer."}]}}"#;

        let graph = convert(input).unwrap();
        let thinking = graph
            .components
            .iter()
            .find(|c| c.entity == "thinking")
            .expect("should have thinking component");
        assert!(thinking.title.starts_with("thinking: Let me think"));
    }

    #[test]
    fn test_skips_malformed_lines() {
        let input = "not json at all\n{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}";
        let graph = convert(input).unwrap();
        assert_eq!(graph.components.len(), 1);
    }

    // ── Metrics tests ───────────────────────────────────────────────

    #[test]
    fn test_metrics_summary() {
        let graph = convert(FIXTURE).unwrap();
        let report = compute_metrics(&graph);

        assert_eq!(report.summary.total_nodes, 6);
        assert_eq!(report.summary.total_edges, 5);

        assert_eq!(report.summary.nodes_by_type["user_message"], 1);
        assert_eq!(report.summary.nodes_by_type["assistant_message"], 2);
        assert_eq!(report.summary.nodes_by_type["tool_use"], 2);
        assert_eq!(report.summary.nodes_by_type["tool_result"], 1);

        assert_eq!(report.summary.edges_by_type["response"], 2);
        assert_eq!(report.summary.edges_by_type["invocation"], 2);
        assert_eq!(report.summary.edges_by_type["result"], 1);
    }

    #[test]
    fn test_metrics_tool_usage() {
        let graph = convert(FIXTURE).unwrap();
        let report = compute_metrics(&graph);

        // Read and Edit, each used once
        assert_eq!(report.tool_usage.len(), 2);
        let names: Vec<&str> = report.tool_usage.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Edit"));
        for entry in &report.tool_usage {
            assert_eq!(entry.count, 1);
        }
    }

    #[test]
    fn test_metrics_fan_out() {
        let graph = convert(FIXTURE).unwrap();
        let report = compute_metrics(&graph);

        // 6 nodes, 5 edges — at least one node has outgoing edges
        assert!(report.fan_out.max >= 1);
        assert!(!report.fan_out.max_node.is_empty());
        assert!(report.fan_out.average > 0.0);
    }

    #[test]
    fn test_metrics_session_stats() {
        let graph = convert(FIXTURE).unwrap();
        let report = compute_metrics(&graph);

        assert_eq!(report.session_stats.user_messages, 1);
        assert_eq!(report.session_stats.assistant_messages, 2);
        assert_eq!(report.session_stats.tool_calls, 2);
        assert_eq!(report.session_stats.thinking_blocks, 0);
        // tool_to_text_ratio = 2 / 3 ≈ 0.67
        assert!(report.session_stats.tool_to_text_ratio > 0.6);
        assert!(report.session_stats.tool_to_text_ratio < 0.7);
    }

    #[test]
    fn test_metrics_patterns_threshold() {
        // Patterns need > 3 repeats. The small fixture won't have any.
        let graph = convert(FIXTURE).unwrap();
        let report = compute_metrics(&graph);
        assert!(report.patterns.is_empty());
    }

    #[test]
    fn test_metrics_patterns_with_repeats() {
        // Build a fixture with repeated Bash -> Bash tool calls (5 times)
        let mut lines = Vec::new();
        // Initial user message
        lines.push(r#"{"type":"user","uuid":"u0","message":{"role":"user","content":[{"type":"text","text":"do stuff"}]}}"#.to_string());
        // 5 rounds of assistant->Bash tool_use followed by tool_result
        for i in 1..=5 {
            lines.push(format!(
                r#"{{"type":"assistant","uuid":"a{i}","message":{{"role":"assistant","content":[{{"type":"text","text":"running"}},{{"type":"tool_use","id":"tu{i}","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
            ));
            lines.push(format!(
                r#"{{"type":"user","uuid":"u{i}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu{i}","content":"ok"}}]}}}}"#
            ));
        }
        let input = lines.join("\n");
        let graph = convert(&input).unwrap();
        let report = compute_metrics(&graph);

        // 4 Bash->Bash bigrams (between the 5 sequential Bash tool_uses)
        let bash_bash = report
            .patterns
            .iter()
            .find(|p| p.from == "Bash" && p.to == "Bash");
        assert!(
            bash_bash.is_some(),
            "expected Bash->Bash pattern, got: {:?}",
            report.patterns
        );
        assert_eq!(bash_bash.unwrap().count, 4);
    }

    #[test]
    fn test_metrics_empty_graph() {
        let graph = convert("").unwrap();
        let report = compute_metrics(&graph);

        assert_eq!(report.summary.total_nodes, 0);
        assert_eq!(report.summary.total_edges, 0);
        assert!(report.tool_usage.is_empty());
        assert_eq!(report.fan_out.max, 0);
        assert_eq!(report.fan_out.average, 0.0);
        assert!(report.patterns.is_empty());
        assert_eq!(report.session_stats.user_messages, 0);
        assert_eq!(report.session_stats.tool_to_text_ratio, 0.0);
    }

    #[test]
    fn test_metrics_with_thinking() {
        let input = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me think..."},{"type":"text","text":"Here is my answer."}]}}"#;

        let graph = convert(input).unwrap();
        let report = compute_metrics(&graph);

        assert_eq!(report.session_stats.thinking_blocks, 1);
        assert_eq!(report.summary.nodes_by_type["thinking"], 1);
    }

    #[test]
    fn test_metrics_json_serialization() {
        let graph = convert(FIXTURE).unwrap();
        let report = compute_metrics(&graph);
        let json = serde_json::to_string_pretty(&report).unwrap();

        // Verify it's valid JSON and contains expected keys
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("summary").is_some());
        assert!(parsed.get("tool_usage").is_some());
        assert!(parsed.get("fan_out").is_some());
        assert!(parsed.get("patterns").is_some());
        assert!(parsed.get("session_stats").is_some());
    }
}
