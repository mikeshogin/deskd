/// MCP (Model Context Protocol) server for deskd.
///
/// Run as: `deskd mcp --agent <name>`
/// Claude invokes this as a subprocess via `--mcp-server "deskd mcp --agent <name>"`.
///
/// Provides tools:
///   - `send_message(target, text)` — publish a message to the agent's bus
///   - `add_persistent_agent(name, model, system_prompt, subscribe)` — launch a new sub-agent
///
/// Environment variables (set by deskd when spawning claude):
///   DESKD_BUS_SOCKET   — Unix socket path of the agent's bus
///   DESKD_AGENT_CONFIG — Path to the agent's deskd.yaml

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{self, UserConfig};

// ─── MCP Protocol types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl Response {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    fn err(id: Option<Value>, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({"code": code, "message": message})),
        }
    }
}

// ─── Main entry point ─────────────────────────────────────────────────────────

/// Run the MCP server for the given agent. Reads JSON-RPC from stdin, writes to stdout.
/// Terminates when stdin closes (i.e. when Claude exits).
pub async fn run(agent_name: &str) -> Result<()> {
    let bus_socket = std::env::var("DESKD_BUS_SOCKET").with_context(|| {
        "DESKD_BUS_SOCKET not set — was this started by deskd?"
    })?;

    let config_path = std::env::var("DESKD_AGENT_CONFIG").ok();
    let user_config = config_path
        .as_deref()
        .and_then(|p| UserConfig::load(p).ok());

    info!(agent = %agent_name, bus = %bus_socket, "MCP server started");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);

    // Detect framing mode from first line of input.
    // Claude Code uses newline-delimited JSON (no Content-Length headers).
    // Older MCP clients use Content-Length framed messages.
    let mut lines = reader.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // stdin closed
            Err(e) => {
                warn!(error = %e, "failed to read line");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Skip Content-Length headers (compat with framed clients).
        if trimmed.starts_with("Content-Length:") {
            continue;
        }

        debug!(msg = %trimmed, "received MCP message");

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, line = %trimmed, "failed to parse JSON-RPC request");
                let resp = Response::err(None, -32700, "Parse error");
                write_response(&mut stdout, &resp).await?;
                continue;
            }
        };

        // Notifications (no id) — acknowledge and continue
        if req.id.is_none() && req.method.starts_with("notifications/") {
            debug!(method = %req.method, "received notification");
            continue;
        }

        let resp = handle_request(&req, agent_name, &bus_socket, user_config.as_ref()).await;
        write_response(&mut stdout, &resp).await?;
    }

    info!(agent = %agent_name, "MCP server stopped (stdin closed)");
    Ok(())
}

// ─── Request routing ──────────────────────────────────────────────────────────

async fn handle_request(
    req: &Request,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => handle_initialize(id),
        "tools/list" => handle_tools_list(id, agent_name, user_config),
        "tools/call" => {
            let params = req.params.as_ref().unwrap_or(&Value::Null);
            match handle_tools_call(params, agent_name, bus_socket, user_config).await {
                Ok(resp) => Response::ok(id, resp),
                Err(e) => Response::err(id, -32603, &format!("{:#}", e)),
            }
        }
        other => {
            debug!(method = %other, "unknown MCP method");
            Response::err(id, -32601, "Method not found")
        }
    }
}

fn handle_initialize(id: Option<Value>) -> Response {
    Response::ok(
        id,
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "deskd",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

fn handle_tools_list(
    id: Option<Value>,
    agent_name: &str,
    user_config: Option<&UserConfig>,
) -> Response {
    let send_message_desc = user_config
        .map(|c| c.send_message_description(agent_name))
        .unwrap_or_else(|| "Send a message to a target on the bus.".to_string());

    let tools = json!([
        {
            "name": "send_message",
            "description": send_message_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Bus target (e.g. agent:dev, telegram.out:-1234, news:ecosystem)"
                    },
                    "text": {
                        "type": "string",
                        "description": "Message content"
                    }
                },
                "required": ["target", "text"]
            }
        },
        {
            "name": "add_persistent_agent",
            "description": "Launch a new persistent sub-agent on this agent's bus. The agent starts immediately and remains connected until the bus shuts down.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Unique name for the new agent"
                    },
                    "model": {
                        "type": "string",
                        "description": "Claude model to use (e.g. claude-sonnet-4-6, claude-haiku-4-5)"
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "System prompt for the new agent"
                    },
                    "subscribe": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Bus targets to subscribe to (e.g. [\"agent:helper\", \"queue:tasks\"])"
                    }
                },
                "required": ["name", "model", "system_prompt", "subscribe"]
            }
        }
    ]);

    Response::ok(id, json!({ "tools": tools }))
}

async fn handle_tools_call(
    params: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .context("missing tool name")?;

    let args = params.get("arguments").unwrap_or(&Value::Null);

    match name {
        "send_message" => call_send_message(args, agent_name, bus_socket, user_config).await,
        "add_persistent_agent" => call_add_persistent_agent(args, agent_name, bus_socket).await,
        other => bail!("Unknown tool: {}", other),
    }
}

// ─── Tool implementations ─────────────────────────────────────────────────────

async fn call_send_message(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let target = args
        .get("target")
        .and_then(|t| t.as_str())
        .context("missing target")?;
    let text = args
        .get("text")
        .and_then(|t| t.as_str())
        .context("missing text")?;

    // Enforce publish allow-list if the calling agent is a sub-agent in config.
    if let Some(cfg) = user_config {
        if let Some(sub) = cfg.agents.iter().find(|a| a.name == agent_name) {
            if let Some(ref allow) = sub.publish {
                let allowed = allow.iter().any(|pattern| glob_match(pattern, target));
                if !allowed {
                    bail!("publish to '{}' not allowed by config", target);
                }
            }
        }
    }

    // Connect to bus, send message, disconnect.
    let mut stream = UnixStream::connect(bus_socket)
        .await
        .with_context(|| format!("failed to connect to bus at {}", bus_socket))?;

    // Register as the agent
    let reg = serde_json::json!({
        "type": "register",
        "name": format!("{}-mcp", agent_name),
        "subscriptions": []
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": agent_name,
        "target": target,
        "payload": {"task": text},
        "metadata": {"priority": 5u8}
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    info!(agent = %agent_name, target = %target, "send_message via MCP");

    Ok(json!({
        "content": [{"type": "text", "text": format!("Message sent to {}", target)}],
        "isError": false
    }))
}

async fn call_add_persistent_agent(
    args: &Value,
    parent_name: &str,
    bus_socket: &str,
) -> Result<Value> {
    let name = args.get("name").and_then(|n| n.as_str()).context("missing name")?;
    let model = args.get("model").and_then(|m| m.as_str()).context("missing model")?;
    let system_prompt = args.get("system_prompt").and_then(|s| s.as_str()).unwrap_or("");
    let subscribe: Vec<String> = args
        .get("subscribe")
        .and_then(|s| s.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
        .unwrap_or_else(|| vec![format!("agent:{}", name)]);

    // Get the deskd binary path (we are running as a subprocess of claude, so $0 is deskd).
    let deskd_bin = std::env::var("DESKD_BIN")
        .unwrap_or_else(|_| "deskd".to_string());

    // Work dir: same as parent (best effort from env or cwd).
    let work_dir = std::env::var("PWD").unwrap_or_else(|_| "/tmp".to_string());

    // Create agent state file via `deskd agent create`.
    let create_status = tokio::process::Command::new(&deskd_bin)
        .args([
            "agent", "create", name,
            "--model", model,
            "--prompt", system_prompt,
            "--workdir", &work_dir,
        ])
        .status()
        .await;

    match create_status {
        Err(e) => {
            anyhow::bail!("failed to create agent '{}': {}", name, e);
        }
        Ok(s) if !s.success() => {
            // Agent may already exist — that's OK for idempotent calls.
            info!(parent = %parent_name, agent = %name, "agent create returned non-zero (may already exist)");
        }
        Ok(_) => {
            info!(parent = %parent_name, agent = %name, model = %model, "agent state created");
        }
    }

    // Start the worker as a background process connected to the parent's bus.
    let _child = tokio::process::Command::new(&deskd_bin)
        .args(["agent", "run", name, "--socket", bus_socket])
        .env("DESKD_BUS_SOCKET", bus_socket)
        .env("DESKD_AGENT_NAME", name)
        .spawn()
        .with_context(|| format!("failed to spawn worker for agent '{}'", name))?;

    // Detach — child runs independently.
    info!(parent = %parent_name, agent = %name, bus = %bus_socket, "persistent sub-agent spawned");

    let subscribe_display = subscribe.join(", ");
    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Agent '{}' started on bus {}. Subscriptions: {}",
                name, bus_socket, subscribe_display
            )
        }],
        "isError": false
    }))
}

// ─── I/O helpers ──────────────────────────────────────────────────────────────

async fn write_response<W: AsyncWriteExt + Unpin>(writer: &mut W, resp: &Response) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    // Newline-delimited JSON (compatible with Claude Code).
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    debug!(resp = %json, "sent MCP response");
    Ok(())
}

// ─── Glob matching ────────────────────────────────────────────────────────────

/// Simple glob matching: `*` matches any sequence of characters except none.
/// `telegram.out:*` matches `telegram.out:-1234567`.
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.splitn(2, '*').collect();
    match parts.as_slice() {
        [prefix, suffix] => value.starts_with(prefix) && value.ends_with(suffix),
        _ => pattern == value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match() {
        assert!(glob_match("agent:*", "agent:dev"));
        assert!(glob_match("telegram.out:*", "telegram.out:-1234567"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("agent:dev", "agent:dev"));
        assert!(!glob_match("agent:dev", "agent:researcher"));
        assert!(!glob_match("telegram.out:*", "telegram.in:-1234"));
    }
}
