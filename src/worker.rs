use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent;
use crate::message::{Message, Metadata};

/// Connect to the bus, register, and return the stream.
pub async fn bus_connect(
    socket_path: &str,
    name: &str,
    subscriptions: Vec<String>,
) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("Failed to connect to bus at {}", socket_path))?;

    let envelope = serde_json::json!({
        "type": "register",
        "name": name,
        "subscriptions": subscriptions,
    });
    let mut line = serde_json::to_string(&envelope)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    info!(agent = %name, "registered on bus");
    Ok(stream)
}

/// Run the agent worker loop: read messages from bus, execute tasks, post results.
/// `bus_socket`: the agent's bus socket path, injected as DESKD_BUS_SOCKET into
/// the claude subprocess so the MCP server can connect to the bus.
pub async fn run(name: &str, socket_path: &str, bus_socket: Option<String>) -> Result<()> {
    let initial_state = agent::load_state(name)?;
    let budget_usd = initial_state.config.budget_usd;

    // Default subscriptions. Workers receive:
    //   agent:<name>     — direct messages (from MCP send_message, other agents, CLI)
    //   queue:tasks      — shared task queue
    //   telegram.in:*    — messages arriving from Telegram adapter
    let subscriptions = vec![
        format!("agent:{}", name),
        "queue:tasks".to_string(),
        "telegram.in:*".to_string(),
    ];

    let stream = bus_connect(socket_path, name, subscriptions).await?;
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    info!(agent = %name, "waiting for tasks");

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }

        let msg: Message = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                warn!(agent = %name, error = %e, "invalid message from bus");
                continue;
            }
        };

        // Check budget against the configured cap.
        let current_state = agent::load_state(name)?;
        if current_state.total_cost >= budget_usd {
            warn!(
                agent = %name,
                cost = current_state.total_cost,
                budget = budget_usd,
                "budget exceeded, rejecting task"
            );
            continue;
        }

        let task = msg.payload.get("task")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        if task.is_empty() {
            debug!(agent = %name, "message has no task payload, skipping");
            continue;
        }

        info!(agent = %name, source = %msg.source, task = %truncate(task, 80), "processing task");

        let max_turns = msg.payload.get("max_turns")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32);

        match agent::send(name, task, max_turns, bus_socket.as_deref()).await {
            Ok(response) => {
                info!(agent = %name, "task completed, posting result");

                let target = msg.reply_to.as_deref().unwrap_or(&msg.source);

                let reply = Message {
                    id: Uuid::new_v4().to_string(),
                    source: name.to_string(),
                    target: target.to_string(),
                    payload: serde_json::json!({
                        "result": response,
                        "in_reply_to": msg.id,
                    }),
                    reply_to: None,
                    metadata: Metadata::default(),
                };

                let envelope = serde_json::json!({
                    "type": "message",
                    "id": reply.id,
                    "source": reply.source,
                    "target": reply.target,
                    "payload": reply.payload,
                    "metadata": reply.metadata,
                });
                let mut reply_line = serde_json::to_string(&envelope)?;
                reply_line.push('\n');

                let mut w = writer.lock().await;
                if let Err(e) = w.write_all(reply_line.as_bytes()).await {
                    warn!(agent = %name, error = %e, target = %target, "failed to write reply to bus");
                } else {
                    debug!(agent = %name, target = %target, "reply sent to bus");
                }
            }
            Err(e) => {
                warn!(agent = %name, error = %e, "task failed");

                if let Some(reply_to) = &msg.reply_to {
                    let error_msg = serde_json::json!({
                        "type": "message",
                        "id": Uuid::new_v4().to_string(),
                        "source": name,
                        "target": reply_to,
                        "payload": {"error": format!("{}", e), "in_reply_to": &msg.id},
                        "metadata": {"priority": 5u8},
                    });
                    let mut err_line = serde_json::to_string(&error_msg)?;
                    err_line.push('\n');

                    let mut w = writer.lock().await;
                    if let Err(write_err) = w.write_all(err_line.as_bytes()).await {
                        warn!(agent = %name, error = %write_err, "failed to write error reply to bus");
                    }
                }
            }
        }
    }

    info!(agent = %name, "disconnected from bus");
    Ok(())
}

/// Send a message via the bus (connect, send, wait for one reply, disconnect).
pub async fn send_via_bus(
    socket_path: &str,
    source: &str,
    target: &str,
    task: &str,
    max_turns: Option<u32>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("Failed to connect to bus at {}", socket_path))?;

    let reg = serde_json::json!({"type": "register", "name": source, "subscriptions": []});
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let mut payload = serde_json::json!({"task": task});
    if let Some(turns) = max_turns {
        payload["max_turns"] = serde_json::json!(turns);
    }

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": source,
        "target": target,
        "payload": payload,
        "reply_to": format!("agent:{}", source),
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    debug!(source = %source, target = %target, "task posted to bus");

    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    if let Some(response_line) = lines.next_line().await? {
        let resp: serde_json::Value = serde_json::from_str(&response_line)?;
        if let Some(result) = resp.get("payload").and_then(|p| p.get("result")).and_then(|r| r.as_str()) {
            println!("{}", result);
        } else if let Some(err) = resp.get("payload").and_then(|p| p.get("error")).and_then(|e| e.as_str()) {
            bail!("Agent error: {}", err);
        } else {
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
