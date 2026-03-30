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
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::UserConfig;
use crate::statemachine;
use crate::unified_inbox;

// ─── MCP Protocol types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
    #[allow(dead_code)]
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
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
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
    let bus_socket = std::env::var("DESKD_BUS_SOCKET")
        .with_context(|| "DESKD_BUS_SOCKET not set — was this started by deskd?")?;

    let config_path = std::env::var("DESKD_AGENT_CONFIG").ok();
    let user_config = config_path
        .as_deref()
        .and_then(|p| UserConfig::load(p).ok());

    info!(agent = %agent_name, bus = %bus_socket, "MCP server started");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);

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

    // Build sm_create description dynamically listing available models.
    let sm_create_desc = if let Some(cfg) = user_config
        && !cfg.models.is_empty()
    {
        let model_list: Vec<String> = cfg
            .models
            .iter()
            .map(|m| {
                if m.description.is_empty() {
                    format!("  {} ({} states)", m.name, m.states.len())
                } else {
                    format!("  {} — {}", m.name, m.description)
                }
            })
            .collect();
        format!(
            "Create a new state machine instance. Available models:\n{}",
            model_list.join("\n")
        )
    } else {
        "Create a new state machine instance.".to_string()
    };

    let mut tools = vec![
        json!({
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
                    },
                    "fresh": {
                        "type": "boolean",
                        "description": "When true, the target agent starts a fresh session for this task (no --resume), regardless of its default session config."
                    }
                },
                "required": ["target", "text"]
            }
        }),
        json!({
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
        }),
    ];

    tools.push(json!({
        "name": "create_reminder",
        "description": "Schedule a one-shot reminder. The message will be posted to the bus target after delay_minutes minutes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Bus target to deliver the reminder to (e.g. agent:kira, telegram.out:-1234)"
                },
                "message": {
                    "type": "string",
                    "description": "Message payload to deliver when the reminder fires"
                },
                "delay_minutes": {
                    "type": "number",
                    "description": "Number of minutes from now to fire the reminder"
                }
            },
            "required": ["target", "message", "delay_minutes"]
        }
    }));

    // Unified inbox tools
    tools.push(json!({
        "name": "list_inboxes",
        "description": "List all unified inboxes with message counts. Returns inbox names (e.g. telegram/12345, github/owner-repo, agent/dev) and how many messages each contains.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    }));
    tools.push(json!({
        "name": "read_inbox",
        "description": "Read messages from a unified inbox. Messages are returned in chronological order (oldest first). Use list_inboxes to discover available inbox names.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "inbox": {
                    "type": "string",
                    "description": "Inbox name (e.g. telegram/12345, github/owner-repo, agent/dev)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of messages to return (default: 50)"
                },
                "since": {
                    "type": "string",
                    "description": "Only return messages after this RFC3339 timestamp (e.g. 2026-03-29T10:00:00Z)"
                }
            },
            "required": ["inbox"]
        }
    }));
    tools.push(json!({
        "name": "search_inbox",
        "description": "Search messages across one or all unified inboxes. Case-insensitive substring match on text, source, and from fields.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "inbox": {
                    "type": "string",
                    "description": "Inbox name to search (omit to search all inboxes)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (substring match)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of results (default: 50)"
                }
            },
            "required": ["query"]
        }
    }));

    // Graph execution tool
    tools.push(json!({
        "name": "run_graph",
        "description": "Execute an executable skill graph from a YAML file. The graph is a DAG of tool call groups and LLM decision nodes. Returns step results and extracted variables.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "description": "Path to the graph YAML file (absolute, or relative to agent work dir)"
                },
                "work_dir": {
                    "type": "string",
                    "description": "Working directory for tool execution (defaults to graph file's parent directory)"
                },
                "vars": {
                    "type": "object",
                    "description": "Input variables as key-value pairs (e.g. {\"pr_number\": \"42\", \"repo\": \"owner/repo\"})"
                }
            },
            "required": ["file"]
        }
    }));

    // Task queue tools
    tools.push(json!({
        "name": "task_create",
        "description": "Create a task in the pull-based task queue. Idle workers matching the criteria will pick it up automatically.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Task description — what the worker should do"
                },
                "model": {
                    "type": "string",
                    "description": "Required model (e.g. claude-sonnet-4-6). Only workers with this model will pick up the task."
                },
                "labels": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Required labels. Only workers with ALL listed labels will pick up the task."
                }
            },
            "required": ["description"]
        }
    }));
    tools.push(json!({
        "name": "task_list",
        "description": "List tasks in the task queue. Returns task ID, status, assignee, and description.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter by status: pending, active, done, failed, cancelled. Omit to list all."
                }
            }
        }
    }));
    tools.push(json!({
        "name": "task_cancel",
        "description": "Cancel a pending task in the task queue. Only pending tasks can be cancelled.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Task ID (e.g. task-a1b2c3d4)"
                }
            },
            "required": ["id"]
        }
    }));
    tools.push(json!({
        "name": "remove_agent",
        "description": "Stop and remove a sub-agent. The agent's worker process is terminated and its state file is deleted.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the agent to remove"
                }
            },
            "required": ["name"]
        }
    }));

    // Add state machine tools if models are defined.
    if user_config.map(|c| !c.models.is_empty()).unwrap_or(false) {
        tools.push(json!({
            "name": "sm_create",
            "description": sm_create_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "Model name (e.g. 'feature', 'bugfix')"},
                    "title": {"type": "string", "description": "Instance title"},
                    "body": {"type": "string", "description": "Instance body/description"}
                },
                "required": ["model", "title"]
            }
        }));
        tools.push(json!({
            "name": "sm_move",
            "description": "Move a state machine instance to a new state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Instance ID (e.g. sm-a1b2c3d4)"},
                    "state": {"type": "string", "description": "Target state name"},
                    "note": {"type": "string", "description": "Optional note/reason"}
                },
                "required": ["id", "state"]
            }
        }));
        tools.push(json!({
            "name": "sm_query",
            "description": "Query state machine instances. Returns JSON list of matching instances.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "Filter by model name"},
                    "state": {"type": "string", "description": "Filter by current state"},
                    "id": {"type": "string", "description": "Get specific instance by ID"}
                }
            }
        }));
    }

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
        "create_reminder" => call_create_reminder(args).await,
        "list_inboxes" => call_list_inboxes().await,
        "read_inbox" => call_read_inbox(args).await,
        "search_inbox" => call_search_inbox(args).await,
        "run_graph" => call_run_graph(args).await,
        "task_create" => call_task_create(args, agent_name).await,
        "task_list" => call_task_list(args).await,
        "task_cancel" => call_task_cancel(args).await,
        "remove_agent" => call_remove_agent(args, agent_name).await,
        "sm_create" => call_sm_create(args, agent_name, bus_socket, user_config).await,
        "sm_move" => call_sm_move(args, agent_name, bus_socket, user_config).await,
        "sm_query" => call_sm_query(args).await,
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
    let fresh = args.get("fresh").and_then(|f| f.as_bool()).unwrap_or(false);

    // Enforce publish allow-list if the calling agent is a sub-agent in config.
    if let Some(cfg) = user_config
        && let Some(sub) = cfg.agents.iter().find(|a| a.name == agent_name)
        && let Some(ref allow) = sub.publish
    {
        let allowed = allow.iter().any(|pattern| glob_match(pattern, target));
        if !allowed {
            bail!("publish to '{}' not allowed by config", target);
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
        "metadata": {"priority": 5u8, "fresh": fresh}
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
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .context("missing name")?;
    let model = args
        .get("model")
        .and_then(|m| m.as_str())
        .context("missing model")?;
    let system_prompt = args
        .get("system_prompt")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let subscribe: Vec<String> = args
        .get("subscribe")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec![format!("agent:{}", name)]);

    // Get the deskd binary path (we are running as a subprocess of claude, so $0 is deskd).
    let deskd_bin = std::env::var("DESKD_BIN").unwrap_or_else(|_| "deskd".to_string());

    // Work dir: same as parent (best effort from env or cwd).
    let work_dir = std::env::var("PWD").unwrap_or_else(|_| "/tmp".to_string());

    // Create agent state file via `deskd agent create`.
    let create_status = tokio::process::Command::new(&deskd_bin)
        .args([
            "agent",
            "create",
            name,
            "--model",
            model,
            "--prompt",
            system_prompt,
            "--workdir",
            &work_dir,
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

    // Set parent field on the created agent state.
    if let Ok(mut state) = crate::agent::load_state(name) {
        state.parent = Some(parent_name.to_string());
        crate::agent::save_state_pub(&state).ok();
    }

    // Start the worker as a background process connected to the parent's bus.
    let mut run_args = vec![
        "agent".to_string(),
        "run".to_string(),
        name.to_string(),
        "--socket".to_string(),
        bus_socket.to_string(),
    ];
    for sub in &subscribe {
        run_args.push("--subscribe".to_string());
        run_args.push(sub.clone());
    }
    let _child = tokio::process::Command::new(&deskd_bin)
        .args(&run_args)
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

// ─── Reminder tool implementation ─────────────────────────────────────────────

async fn call_create_reminder(args: &Value) -> Result<Value> {
    let target = args
        .get("target")
        .and_then(|t| t.as_str())
        .context("missing target")?;
    let message = args
        .get("message")
        .and_then(|m| m.as_str())
        .context("missing message")?;
    let delay_minutes = args
        .get("delay_minutes")
        .and_then(|d| d.as_f64())
        .context("missing delay_minutes")?;

    let fire_at =
        chrono::Utc::now() + chrono::Duration::seconds((delay_minutes * 60.0).round() as i64);

    let remind = crate::config::RemindDef {
        at: fire_at.to_rfc3339(),
        target: target.to_string(),
        message: message.to_string(),
    };

    let dir = crate::config::reminders_dir();
    let filename = format!("{}.json", uuid::Uuid::new_v4());
    let path = dir.join(&filename);

    let json = serde_json::to_string_pretty(&remind).context("failed to serialize reminder")?;
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write reminder file: {}", path.display()))?;

    info!(target = %target, at = %fire_at, "create_reminder via MCP");

    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Reminder scheduled: target={} at={} (in {:.0} minutes)",
                target,
                fire_at.to_rfc3339(),
                delay_minutes
            )
        }],
        "isError": false
    }))
}

// ─── Unified inbox tool implementations ──────────────────────────────────────

async fn call_list_inboxes() -> Result<Value> {
    let inboxes = unified_inbox::list_inboxes().context("failed to list inboxes")?;

    let summary: Vec<Value> = inboxes
        .iter()
        .map(|(name, count)| {
            json!({
                "inbox": name,
                "messages": count,
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&summary)?}],
        "isError": false
    }))
}

async fn call_read_inbox(args: &Value) -> Result<Value> {
    let inbox = args
        .get("inbox")
        .and_then(|i| i.as_str())
        .context("missing inbox")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
    let since = args
        .get("since")
        .and_then(|s| s.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let messages =
        unified_inbox::read_messages(inbox, limit, since).context("failed to read inbox")?;

    let output: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "ts": m.ts.to_rfc3339(),
                "source": m.source,
                "from": m.from,
                "text": m.text,
                "metadata": m.metadata,
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&output)?}],
        "isError": false
    }))
}

async fn call_search_inbox(args: &Value) -> Result<Value> {
    let inbox = args.get("inbox").and_then(|i| i.as_str());
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .context("missing query")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;

    let results =
        unified_inbox::search_messages(inbox, query, limit).context("failed to search inbox")?;

    let output: Vec<Value> = results
        .iter()
        .map(|m| {
            json!({
                "ts": m.ts.to_rfc3339(),
                "source": m.source,
                "from": m.from,
                "text": m.text,
                "metadata": m.metadata,
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&output)?}],
        "isError": false
    }))
}

// ─── Graph execution tool implementation ─────────────────────────────────────

async fn call_run_graph(args: &Value) -> Result<Value> {
    let file = args
        .get("file")
        .and_then(|f| f.as_str())
        .context("missing file")?;

    let file_path = std::path::Path::new(file);
    let abs_path = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        let cwd = std::env::var("PWD").unwrap_or_else(|_| ".".to_string());
        std::path::Path::new(&cwd).join(file_path)
    };

    let work_dir = if let Some(wd) = args.get("work_dir").and_then(|w| w.as_str()) {
        std::path::PathBuf::from(wd)
    } else {
        abs_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf()
    };

    let yaml = std::fs::read_to_string(&abs_path)
        .with_context(|| format!("failed to read graph file: {}", abs_path.display()))?;
    let graph_def: crate::graph::GraphDef = serde_yaml::from_str(&yaml)
        .with_context(|| format!("failed to parse graph YAML: {}", abs_path.display()))?;

    // Parse optional vars from MCP args.
    let inputs: Option<std::collections::HashMap<String, String>> =
        args.get("vars").and_then(|v| v.as_object()).map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        });

    info!(graph = %graph_def.graph, steps = graph_def.steps.len(), "run_graph via MCP");

    let ctx = crate::graph::execute(&graph_def, &work_dir, None, inputs)
        .await
        .with_context(|| format!("graph execution failed: {}", graph_def.graph))?;

    // Build a summary of results.
    let mut summary = format!(
        "Graph '{}' completed: {} steps executed\n",
        graph_def.graph,
        ctx.results.len()
    );

    for step in &graph_def.steps {
        if let Some(result) = ctx.results.get(&step.id) {
            let status = if result.skipped { "SKIP" } else { "DONE" };
            summary.push_str(&format!(
                "  [{status}] {} ({}ms)\n",
                result.id, result.duration_ms
            ));

            // Include non-empty tool outputs (truncated).
            for tr in &result.tool_results {
                if !tr.stdout.is_empty() && !tr.skipped {
                    let output = if tr.stdout.len() > 500 {
                        format!("{}...", &tr.stdout[..500])
                    } else {
                        tr.stdout.clone()
                    };
                    summary.push_str(&format!("    {}: {}\n", tr.tool, output.trim()));
                }
                if !tr.stderr.is_empty() && tr.exit_code != 0 {
                    let err_output = if tr.stderr.len() > 200 {
                        format!("{}...", &tr.stderr[..200])
                    } else {
                        tr.stderr.clone()
                    };
                    summary.push_str(&format!("    {} stderr: {}\n", tr.tool, err_output.trim()));
                }
            }

            // Include LLM decision output (truncated).
            if let Some(ref llm_out) = result.llm_output {
                let display = if llm_out.len() > 500 {
                    format!("{}...", &llm_out[..500])
                } else {
                    llm_out.clone()
                };
                summary.push_str(&format!("    LLM: {}\n", display.trim()));
            }
        }
    }

    if !ctx.variables.is_empty() {
        summary.push_str("\nVariables:\n");
        for (k, v) in &ctx.variables {
            let display = if v.len() > 200 {
                format!("{}...", &v[..200])
            } else {
                v.clone()
            };
            summary.push_str(&format!("  {}: {}\n", k, display));
        }
    }

    Ok(json!({
        "content": [{"type": "text", "text": summary}],
        "isError": false
    }))
}

// ─── Task queue tool implementations ─────────────────────────────────────────

async fn call_task_create(args: &Value, agent_name: &str) -> Result<Value> {
    let description = args
        .get("description")
        .and_then(|d| d.as_str())
        .context("missing description")?;
    let model = args.get("model").and_then(|m| m.as_str()).map(String::from);
    let labels: Vec<String> = args
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let store = crate::task::TaskStore::default_for_home();
    let criteria = crate::task::TaskCriteria { model, labels };
    let task = store.create(description, criteria, agent_name)?;

    info!(agent = %agent_name, task_id = %task.id, "task_create via MCP");

    Ok(json!({
        "content": [{"type": "text", "text": format!(
            "Task created: {} (status=pending, id={})", task.description, task.id
        )}],
        "isError": false
    }))
}

async fn call_task_list(args: &Value) -> Result<Value> {
    let status_filter = args
        .get("status")
        .and_then(|s| s.as_str())
        .and_then(|s| match s {
            "pending" => Some(crate::task::TaskStatus::Pending),
            "active" => Some(crate::task::TaskStatus::Active),
            "done" => Some(crate::task::TaskStatus::Done),
            "failed" => Some(crate::task::TaskStatus::Failed),
            "cancelled" => Some(crate::task::TaskStatus::Cancelled),
            _ => None,
        });

    let store = crate::task::TaskStore::default_for_home();
    let tasks = store.list(status_filter)?;

    let summary: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "description": t.description,
                "status": t.status,
                "assignee": t.assignee,
                "created_by": t.created_by,
                "created_at": t.created_at,
                "sm_instance_id": t.sm_instance_id,
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&summary)?}],
        "isError": false
    }))
}

async fn call_task_cancel(args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .context("missing id")?;

    let store = crate::task::TaskStore::default_for_home();
    let task = store.cancel(id)?;

    info!(task_id = %task.id, "task_cancel via MCP");

    Ok(json!({
        "content": [{"type": "text", "text": format!("Task {} cancelled", task.id)}],
        "isError": false
    }))
}

async fn call_remove_agent(args: &Value, caller: &str) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .context("missing name")?;

    // Verify the target agent exists and is a sub-agent of the caller.
    let state =
        crate::agent::load_state(name).with_context(|| format!("agent '{}' not found", name))?;

    match &state.parent {
        Some(parent) if parent == caller => {}
        _ => bail!(
            "agent '{}' is not a sub-agent of '{}' — removal denied",
            name,
            caller
        ),
    }

    // Graceful shutdown: send SIGTERM and wait for process to exit.
    if state.pid > 0 {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &state.pid.to_string()])
            .status();
        info!(agent = %name, pid = state.pid, "sent SIGTERM, waiting for exit");

        // Wait up to 30 seconds for the process to exit.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            // Check if process is still alive (kill -0).
            let alive = std::process::Command::new("kill")
                .args(["-0", &state.pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                warn!(agent = %name, pid = state.pid, "process did not exit in 30s, force killing");
                let _ = std::process::Command::new("kill")
                    .args(["-9", &state.pid.to_string()])
                    .status();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    crate::agent::remove(name).await?;

    info!(agent = %name, caller = %caller, "remove_agent via MCP");

    Ok(json!({
        "content": [{"type": "text", "text": format!("Agent '{}' removed", name)}],
        "isError": false
    }))
}

// ─── State machine tool implementations ───────────────────────────────────────

async fn call_sm_create(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let model_name = args
        .get("model")
        .and_then(|m| m.as_str())
        .context("missing model")?;
    let title = args
        .get("title")
        .and_then(|t| t.as_str())
        .context("missing title")?;
    let body = args.get("body").and_then(|b| b.as_str()).unwrap_or("");

    let cfg = user_config.context("no user config loaded — models not available")?;
    let model = cfg
        .models
        .iter()
        .find(|m| m.name == model_name)
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found", model_name))?;

    let store = statemachine::StateMachineStore::default_for_home();
    let inst = store.create(model, title, body, agent_name)?;
    info!(agent = %agent_name, instance = %inst.id, model = %model_name, "sm_create via MCP");

    // If the initial state has an assignee, dispatch the first task via the bus.
    if !inst.assignee.is_empty() {
        let task_text = format!(
            "---\n## Task: {}\n\n{}\n\n---\n## Metadata\ninstance_id: {}\nmodel: {}\nstate: {}",
            inst.title, inst.body, inst.id, inst.model, inst.state
        );

        let mut stream = UnixStream::connect(bus_socket)
            .await
            .with_context(|| format!("failed to connect to bus at {}", bus_socket))?;

        let reg = serde_json::json!({
            "type": "register",
            "name": format!("{}-mcp-sm", agent_name),
            "subscriptions": []
        });
        let mut line = serde_json::to_string(&reg)?;
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;

        let msg = serde_json::json!({
            "type": "message",
            "id": Uuid::new_v4().to_string(),
            "source": "workflow-engine",
            "target": &inst.assignee,
            "payload": {
                "task": task_text,
                "sm_instance_id": inst.id,
            },
            "reply_to": format!("sm:{}", inst.id),
            "metadata": {"priority": 5u8},
        });
        let mut msg_line = serde_json::to_string(&msg)?;
        msg_line.push('\n');
        stream.write_all(msg_line.as_bytes()).await?;
        info!(instance = %inst.id, assignee = %inst.assignee, "dispatched initial task");
    }

    Ok(json!({
        "content": [{"type": "text", "text": format!(
            "Created instance {} (model={}, state={}, assignee={})",
            inst.id, inst.model, inst.state, inst.assignee
        )}],
        "isError": false
    }))
}

async fn call_sm_move(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .context("missing id")?;
    let state = args
        .get("state")
        .and_then(|s| s.as_str())
        .context("missing state")?;
    let note = args.get("note").and_then(|n| n.as_str());

    let store = statemachine::StateMachineStore::default_for_home();
    let mut inst = store.load(id)?;
    let cfg = user_config.context("no user config loaded — models not available")?;
    let model = cfg
        .models
        .iter()
        .find(|m| m.name == inst.model)
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found in config", inst.model))?;

    let from = inst.state.clone();
    store.move_to(&mut inst, model, state, agent_name, note)?;
    info!(agent = %agent_name, instance = %id, from = %from, to = %state, "sm_move via MCP");

    // If the new state has an assignee, dispatch via bus.
    if !inst.assignee.is_empty() && !statemachine::is_terminal(model, &inst) {
        let task_text = format!(
            "---\n## Task: {}\n\n{}\n\n---\n## Metadata\ninstance_id: {}\nmodel: {}\nstate: {}",
            inst.title, inst.body, inst.id, inst.model, inst.state
        );

        let mut stream = UnixStream::connect(bus_socket)
            .await
            .with_context(|| format!("failed to connect to bus at {}", bus_socket))?;

        let reg = serde_json::json!({
            "type": "register",
            "name": format!("{}-mcp-sm", agent_name),
            "subscriptions": []
        });
        let mut line = serde_json::to_string(&reg)?;
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;

        let msg = serde_json::json!({
            "type": "message",
            "id": Uuid::new_v4().to_string(),
            "source": "workflow-engine",
            "target": &inst.assignee,
            "payload": {
                "task": task_text,
                "sm_instance_id": inst.id,
            },
            "reply_to": format!("sm:{}", inst.id),
            "metadata": {"priority": 5u8},
        });
        let mut msg_line = serde_json::to_string(&msg)?;
        msg_line.push('\n');
        stream.write_all(msg_line.as_bytes()).await?;
    }

    Ok(json!({
        "content": [{"type": "text", "text": format!("{} → {} (model={})", id, inst.state, inst.model)}],
        "isError": false
    }))
}

async fn call_sm_query(args: &Value) -> Result<Value> {
    // If a specific ID is requested, return just that instance.
    if let Some(id) = args.get("id").and_then(|i| i.as_str()) {
        let store = statemachine::StateMachineStore::default_for_home();
        let inst = store.load(id)?;
        let inst_json = serde_json::to_value(&inst)?;
        return Ok(json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&inst_json)?}],
            "isError": false
        }));
    }

    let store = statemachine::StateMachineStore::default_for_home();
    let mut instances = store.list_all()?;

    if let Some(model_filter) = args.get("model").and_then(|m| m.as_str()) {
        instances.retain(|i| i.model == model_filter);
    }
    if let Some(state_filter) = args.get("state").and_then(|s| s.as_str()) {
        instances.retain(|i| i.state == state_filter);
    }

    let summary: Vec<Value> = instances
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "model": i.model,
                "title": i.title,
                "state": i.state,
                "assignee": i.assignee,
                "updated_at": i.updated_at,
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&summary)?}],
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
