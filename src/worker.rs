use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent;
use crate::inbox;
use crate::message::Message;

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

/// Helper: write an inbox entry for a completed task.
fn write_inbox(
    name: &str,
    msg: &Message,
    task: &str,
    result: Option<String>,
    error: Option<String>,
) {
    let entry = inbox::InboxEntry {
        id: msg.id.clone(),
        agent: name.to_string(),
        source: msg.source.clone(),
        task: task.to_string(),
        result,
        error,
        in_reply_to: msg.id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    // Write to the sender's inbox (e.g. "cli"), so they can read replies.
    if let Err(e) = inbox::write(&msg.source, &entry) {
        warn!(agent = %name, error = %e, "failed to write inbox entry");
    }
}

/// Run the agent worker loop: read messages from bus, execute tasks, post results.
/// `bus_socket`: the agent's bus socket path, injected as DESKD_BUS_SOCKET into
/// the claude subprocess so the MCP server can connect to the bus.
pub async fn run(
    name: &str,
    socket_path: &str,
    bus_socket: Option<String>,
    subscriptions: Option<Vec<String>>,
) -> Result<()> {
    let initial_state = agent::load_state(name)?;
    let budget_usd = initial_state.config.budget_usd;

    // Use custom subscriptions if provided, otherwise default.
    // Default subscriptions: Workers receive:
    //   agent:<name>     — direct messages (from MCP send_message, other agents, CLI)
    //   queue:tasks      — shared task queue
    //   telegram.in:*    — messages arriving from Telegram adapter
    let subscriptions = subscriptions.unwrap_or_else(|| {
        vec![
            format!("agent:{}", name),
            "queue:tasks".to_string(),
            "telegram.in:*".to_string(),
        ]
    });

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

        let task_raw = msg
            .payload
            .get("task")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        if task_raw.is_empty() {
            debug!(agent = %name, "message has no task payload, skipping");
            continue;
        }

        // Extract optional image data from payload (sent by Telegram adapter for photos).
        let image_base64 = msg
            .payload
            .get("image_base64")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let image_media_type = msg
            .payload
            .get("image_media_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Prepend channel context so the agent knows where the message came from.
        let task_owned;
        let task =
            if let Some(chat_id) = msg.payload.get("telegram_chat_id").and_then(|v| v.as_i64()) {
                let label = msg
                    .payload
                    .get("telegram_chat_name")
                    .and_then(|v| v.as_str())
                    .map(|n| format!("{} ({})", n, chat_id))
                    .unwrap_or_else(|| chat_id.to_string());
                task_owned = format!("[Telegram: {}]\n{}", label, task_raw);
                &task_owned as &str
            } else {
                task_raw
            };

        info!(agent = %name, source = %msg.source, task = %truncate(task, 80), "processing task");

        // Mark agent as working so `deskd status` can report live state.
        if let Ok(mut st) = agent::load_state(name) {
            st.status = "working".to_string();
            st.current_task = truncate(task, 80).to_string();
            let _ = agent::save_state_pub(&st);
        }

        let max_turns = msg
            .payload
            .get("max_turns")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32);

        // Determine reply target: workflow engine tasks route back to sm:<id>.
        let reply_target = if let Some(sm_id) =
            msg.payload.get("sm_instance_id").and_then(|v| v.as_str())
        {
            format!("sm:{}", sm_id)
        } else {
            msg.reply_to.as_deref().unwrap_or(&msg.source).to_string()
        };
        let is_telegram = reply_target.starts_with("telegram.out:");
        let telegram_chat_id = if is_telegram {
            reply_target
                .strip_prefix("telegram.out:")
                .and_then(|id| id.parse::<i64>().ok())
        } else {
            None
        };

        // Telegram-specific: typing indicators + progress message timer.
        let progress_cancel_tx = if let Some(chat_id) = telegram_chat_id {
            let ctrl_target = format!("telegram.ctrl:{}", chat_id);

            // Signal typing start
            write_bus_envelope(
                &writer,
                name,
                &ctrl_target,
                serde_json::json!({"typing": true}),
            )
            .await;

            // Send initial progress message
            write_bus_envelope(
                &writer,
                name,
                &ctrl_target,
                serde_json::json!({"progress_start": true}),
            )
            .await;

            // Spawn a task that edits the progress message every 5 seconds
            let start_time = std::time::Instant::now();
            let progress_writer = writer.clone();
            let progress_name = name.to_string();
            let progress_ctrl = ctrl_target;
            let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.tick().await; // skip first immediate tick
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let elapsed = start_time.elapsed().as_secs();
                            let text = format!("⏳ Working... {}s", elapsed);
                            write_bus_envelope(
                                &progress_writer,
                                &progress_name,
                                &progress_ctrl,
                                serde_json::json!({"progress_update": text}),
                            )
                            .await;
                        }
                        _ = &mut cancel_rx => break,
                    }
                }
            });

            Some(cancel_tx)
        } else {
            None
        };

        // Unified streaming path: always use send_streaming() for all targets.
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let writer_fwd = writer.clone();
        let name_owned = name.to_string();
        let reply_owned = reply_target.clone();
        let fwd_task = tokio::spawn(async move {
            let mut full_response = String::new();
            while let Some(text) = progress_rx.recv().await {
                full_response.push_str(&text);
                write_bus_envelope(
                    &writer_fwd,
                    &name_owned,
                    &reply_owned,
                    serde_json::json!({"result": text}),
                )
                .await;
            }
            full_response
        });

        // Create injection channel for mid-task message forwarding.
        let (inject_tx, inject_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let image = image_base64.as_deref().zip(image_media_type.as_deref());

        // Pin the task future so we can poll it in select!
        let mut task_fut = Box::pin(agent::send_streaming(
            name,
            task,
            max_turns,
            bus_socket.as_deref(),
            progress_tx,
            image,
            Some(inject_rx),
        ));

        // Concurrently await task completion OR new bus messages for injection.
        let result = loop {
            tokio::select! {
                task_result = &mut task_fut => {
                    break task_result;
                }
                Ok(Some(bus_line)) = lines.next_line() => {
                    if bus_line.is_empty() {
                        continue;
                    }
                    if let Ok(inject_msg) = serde_json::from_str::<Message>(&bus_line) {
                        let inject_task = inject_msg
                            .payload
                            .get("task")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default();
                        if !inject_task.is_empty() {
                            info!(
                                agent = %name,
                                source = %inject_msg.source,
                                task = %truncate(inject_task, 80),
                                "injecting mid-task message"
                            );
                            let _ = inject_tx.send(inject_task.to_string());
                        }
                    } else {
                        warn!(agent = %name, "invalid message from bus during task, skipping");
                    }
                }
            }
        };
        // Dropping inject_tx signals the inject forwarder in agent.rs to exit.
        drop(inject_tx);

        let full_response = fwd_task.await.unwrap_or_default();

        // Telegram-specific: cancel progress timer, signal typing stop.
        if let Some(cancel_tx) = progress_cancel_tx {
            let _ = cancel_tx.send(());
            if let Some(chat_id) = telegram_chat_id {
                let ctrl_target = format!("telegram.ctrl:{}", chat_id);
                write_bus_envelope(
                    &writer,
                    name,
                    &ctrl_target,
                    serde_json::json!({"progress_done": true}),
                )
                .await;
                write_bus_envelope(
                    &writer,
                    name,
                    &ctrl_target,
                    serde_json::json!({"typing": false}),
                )
                .await;
            }
        }

        set_idle(name);
        match result {
            Ok(_) => {
                info!(agent = %name, "task completed (streamed)");

                // Write to file-based inbox for all senders.
                if !full_response.is_empty() {
                    write_inbox(name, &msg, task, Some(full_response), None);
                }

                // Send final completion marker to reply_to.
                write_bus_envelope(
                    &writer,
                    name,
                    &reply_target,
                    serde_json::json!({"final": true, "in_reply_to": msg.id}),
                )
                .await;
            }
            Err(e) => {
                warn!(agent = %name, error = %e, "task failed");
                write_inbox(name, &msg, task, None, Some(format!("{}", e)));
                write_bus_envelope(
                    &writer,
                    name,
                    &reply_target,
                    serde_json::json!({"error": format!("{}", e), "in_reply_to": msg.id}),
                )
                .await;
            }
        }
    }

    info!(agent = %name, "disconnected from bus");
    Ok(())
}

/// Mark agent as idle in its state file.
fn set_idle(name: &str) {
    if let Ok(mut st) = agent::load_state(name) {
        st.status = "idle".to_string();
        st.current_task = String::new();
        let _ = agent::save_state_pub(&st);
    }
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
        if let Some(result) = resp
            .get("payload")
            .and_then(|p| p.get("result"))
            .and_then(|r| r.as_str())
        {
            println!("{}", result);
        } else if let Some(err) = resp
            .get("payload")
            .and_then(|p| p.get("error"))
            .and_then(|e| e.as_str())
        {
            bail!("Agent error: {}", err);
        } else {
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
    }

    Ok(())
}

/// Write a bus message envelope to the shared writer.
async fn write_bus_envelope(
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    source: &str,
    target: &str,
    payload: serde_json::Value,
) {
    let envelope = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": source,
        "target": target,
        "payload": payload,
        "metadata": {"priority": 5u8},
    });
    let Ok(mut line) = serde_json::to_string(&envelope) else {
        return;
    };
    line.push('\n');
    let mut w = writer.lock().await;
    let _ = w.write_all(line.as_bytes()).await;
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
