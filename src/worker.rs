use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
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

    eprintln!("[worker] registered as {} on bus", name);
    Ok(stream)
}

/// Run the agent worker loop: read messages from bus, execute tasks, post results.
pub async fn run(name: &str, socket_path: &str) -> Result<()> {
    agent::load_state(name)?; // verify agent exists
    let max_cost = 50.0_f64; // budget cap in USD

    // Subscribe to agent-specific target and the default task queue
    let subscriptions = vec![
        format!("agent:{}", name),
        "queue:tasks".to_string(),
    ];

    let stream = bus_connect(socket_path, name, subscriptions).await?;
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    eprintln!("[worker] {} waiting for tasks", name);

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }

        let msg: Message = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[worker] invalid message: {}", e);
                continue;
            }
        };

        // Check budget
        let current_state = agent::load_state(name)?;
        if current_state.total_cost >= max_cost {
            eprintln!("[worker] budget exceeded (${:.2} >= ${:.2}), rejecting task", current_state.total_cost, max_cost);
            continue;
        }

        let task = msg.payload.get("task")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        if task.is_empty() {
            eprintln!("[worker] message has no task payload, skipping");
            continue;
        }

        eprintln!("[worker] processing task from {}: {}", msg.source, truncate(task, 80));

        // Run claude via existing send() logic
        let max_turns = msg.payload.get("max_turns")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32);

        match agent::send(name, task, max_turns).await {
            Ok(response) => {
                eprintln!("[worker] task completed, posting result");

                // Determine reply target
                let target = msg.reply_to
                    .as_deref()
                    .unwrap_or(&msg.source);

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

                let envelope = serde_json::json!({"type": "message", "id": reply.id, "source": reply.source, "target": reply.target, "payload": reply.payload, "metadata": reply.metadata});
                let mut reply_line = serde_json::to_string(&envelope)?;
                reply_line.push('\n');

                let mut w = writer.lock().await;
                match w.write_all(reply_line.as_bytes()).await {
                    Ok(_) => eprintln!("[worker] reply sent to bus: target={}", target),
                    Err(e) => eprintln!("[worker] failed to write reply to bus: {}", e),
                }
            }
            Err(e) => {
                eprintln!("[worker] task failed: {}", e);

                // Post error back if there's a reply target
                if let Some(reply_to) = &msg.reply_to {
                    let reply = serde_json::json!({
                        "type": "message",
                        "id": Uuid::new_v4().to_string(),
                        "source": name,
                        "target": reply_to,
                        "payload": {"error": format!("{}", e), "in_reply_to": &msg.id},
                        "metadata": {"priority": 5u8},
                    });
                    let mut reply_line = serde_json::to_string(&reply)?;
                    reply_line.push('\n');

                    let mut w = writer.lock().await;
                    w.write_all(reply_line.as_bytes()).await.ok();
                }
            }
        }
    }

    eprintln!("[worker] disconnected from bus");
    Ok(())
}

/// Send a message via the bus (connect, send, disconnect).
pub async fn send_via_bus(socket_path: &str, source: &str, target: &str, task: &str, max_turns: Option<u32>) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("Failed to connect to bus at {}", socket_path))?;

    // Register as a transient client
    let reg = serde_json::json!({"type": "register", "name": source, "subscriptions": []});
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    // Send the task message
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

    eprintln!("[send] task posted to {} via bus", target);

    // Read response (blocking wait for one message back)
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
    // Find the last char boundary at or before max
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
