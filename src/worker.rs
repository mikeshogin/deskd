use anyhow::{Context, Result, bail};
use std::io::Write;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::acp;
use crate::agent::{self, TokenUsage};
use crate::config::{AgentRuntime, SessionMode};
use crate::inbox;
use crate::message::Message;
use crate::tasklog;
use crate::unified_inbox;

/// Wrapper for either a Claude or ACP agent process.
enum RuntimeProcess {
    Claude(agent::AgentProcess),
    Acp(acp::AcpProcess),
}

impl RuntimeProcess {
    async fn start(name: &str, bus_socket: &str, runtime: &AgentRuntime) -> Result<Self> {
        match runtime {
            AgentRuntime::Claude => {
                let p = agent::AgentProcess::start(name, bus_socket).await?;
                Ok(Self::Claude(p))
            }
            AgentRuntime::Acp => {
                let p = acp::AcpProcess::start(name, bus_socket).await?;
                Ok(Self::Acp(p))
            }
        }
    }

    async fn start_fresh(name: &str, bus_socket: &str, runtime: &AgentRuntime) -> Result<Self> {
        match runtime {
            AgentRuntime::Claude => {
                let p = agent::AgentProcess::start_fresh(name, bus_socket).await?;
                Ok(Self::Claude(p))
            }
            AgentRuntime::Acp => {
                let p = acp::AcpProcess::start_fresh(name, bus_socket).await?;
                Ok(Self::Acp(p))
            }
        }
    }

    async fn send_task(
        &self,
        message: &str,
        progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
        image: Option<(&str, &str)>,
        limits: &agent::TaskLimits,
    ) -> Result<agent::TurnResult> {
        match self {
            Self::Claude(p) => p.send_task(message, progress_tx, image, limits).await,
            Self::Acp(p) => {
                // ACP doesn't support image content — include note if image was provided.
                let effective_message = if image.is_some() {
                    format!(
                        "[Note: image attachment not supported via ACP]\n{}",
                        message
                    )
                } else {
                    message.to_string()
                };
                p.send_task(&effective_message, progress_tx, limits).await
            }
        }
    }

    fn inject_message(&self, message: &str) -> Result<()> {
        match self {
            Self::Claude(p) => p.inject_message(message),
            Self::Acp(_) => {
                // ACP doesn't support mid-task message injection.
                warn!("ACP runtime does not support mid-task message injection");
                Ok(())
            }
        }
    }

    async fn stop(&self) {
        match self {
            Self::Claude(p) => p.stop().await,
            Self::Acp(p) => p.stop().await,
        }
    }
}

/// Connect to the bus, register, and return the stream.
///
/// Retries up to 10 times with exponential backoff (100ms initial delay,
/// doubling each attempt) to handle the race where the worker starts
/// before the bus is listening on the socket.
pub async fn bus_connect(
    socket_path: &str,
    name: &str,
    subscriptions: Vec<String>,
) -> Result<UnixStream> {
    let max_retries = 10u32;
    let initial_delay = std::time::Duration::from_millis(100);

    let mut stream = None;
    let mut last_err = None;
    for attempt in 0..max_retries {
        match UnixStream::connect(socket_path).await {
            Ok(s) => {
                if attempt > 0 {
                    info!(agent = %name, attempt = attempt + 1, "connected to bus after retry");
                }
                stream = Some(s);
                break;
            }
            Err(e) => {
                if attempt + 1 < max_retries {
                    let delay = initial_delay * 2u32.saturating_pow(attempt);
                    warn!(
                        agent = %name,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "bus not ready, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
                last_err = Some(e);
            }
        }
    }

    let mut stream = match stream {
        Some(s) => s,
        None => {
            return Err(last_err.unwrap()).with_context(|| {
                format!(
                    "Failed to connect to bus at {} after {} attempts",
                    socket_path, max_retries
                )
            });
        }
    };

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

    // Start persistent agent process (reused across tasks).
    let effective_bus = bus_socket.as_deref().unwrap_or(socket_path).to_string();
    let agent_runtime = initial_state.config.runtime.clone();
    let mut process = RuntimeProcess::start(name, &effective_bus, &agent_runtime).await?;

    // Build task limits from agent config — enforced in real-time during tasks.
    let limits = agent::TaskLimits {
        max_turns: if initial_state.config.max_turns > 0 {
            Some(initial_state.config.max_turns)
        } else {
            None
        },
        budget_usd: Some(budget_usd),
    };

    info!(agent = %name, runtime = ?agent_runtime, "agent process ready, waiting for tasks");

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

            // Log budget skip.
            let skip_task = msg
                .payload
                .get("task")
                .and_then(|t| t.as_str())
                .unwrap_or_default();
            let log_entry = tasklog::TaskLog {
                ts: chrono::Utc::now().to_rfc3339(),
                source: msg.source.clone(),
                turns: 0,
                cost: 0.0,
                duration_ms: 0,
                status: "skip".to_string(),
                task: tasklog::truncate_task(skip_task, 60),
                error: Some("budget exceeded".to_string()),
                msg_id: msg.id.clone(),
            };
            if let Err(e) = tasklog::log_task(name, &log_entry) {
                warn!(agent = %name, error = %e, "failed to write task log");
            }

            // Notify the sender so they get a visible error instead of silence.
            let budget_error = format!(
                "Budget limit reached (${:.2} / ${:.2}). Task not processed.",
                current_state.total_cost, budget_usd,
            );
            let reply_target = msg.reply_to.as_deref().unwrap_or(&msg.source);
            write_bus_envelope(
                &writer,
                name,
                reply_target,
                serde_json::json!({"error": budget_error, "in_reply_to": msg.id}),
            )
            .await;

            continue;
        }

        let task_raw = msg
            .payload
            .get("task")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        if task_raw.is_empty() {
            debug!(agent = %name, "message has no task payload, skipping");
            let log_entry = tasklog::TaskLog {
                ts: chrono::Utc::now().to_rfc3339(),
                source: msg.source.clone(),
                turns: 0,
                cost: 0.0,
                duration_ms: 0,
                status: "empty".to_string(),
                task: String::new(),
                error: None,
                msg_id: msg.id.clone(),
            };
            if let Err(e) = tasklog::log_task(name, &log_entry) {
                warn!(agent = %name, error = %e, "failed to write task log");
            }
            continue;
        }

        // Write to unified inbox for inter-agent / bus messages.
        // Telegram messages are already written by the telegram adapter,
        // so skip those to avoid duplicates.
        if !msg.source.starts_with("telegram-") {
            let inbox_name = format!("agent/{}", name);
            let inbox_msg = unified_inbox::InboxMessage {
                ts: chrono::Utc::now(),
                source: msg.source.clone(),
                from: None,
                text: task_raw.to_string(),
                metadata: serde_json::json!({
                    "target": msg.target,
                    "message_id": msg.id,
                }),
            };
            if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
                warn!(agent = %name, error = %e, "failed to write to unified inbox");
            }
        }

        // Check if this task requests a fresh session (via metadata.fresh flag).
        // Also check if the agent's session mode is ephemeral.
        let needs_fresh =
            msg.metadata.fresh || initial_state.config.session == SessionMode::Ephemeral;
        if needs_fresh {
            info!(agent = %name, fresh = msg.metadata.fresh, "fresh session requested, restarting process");
            process.stop().await;
            process = RuntimeProcess::start_fresh(name, &effective_bus, &agent_runtime).await?;
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
                // Non-Telegram sources: tag with source so agent can distinguish
                // automated messages (schedules, other agents) from user messages.
                task_owned = format!("[source: {}]\n{}", msg.source, task_raw);
                &task_owned as &str
            };

        info!(agent = %name, source = %msg.source, task = %truncate(task, 80), "processing task");
        let _task_start = std::time::Instant::now();

        // Mark agent as working so `deskd status` can report live state.
        if let Ok(mut st) = agent::load_state(name) {
            st.status = "working".to_string();
            st.current_task = truncate(task, 80).to_string();
            let _ = agent::save_state_pub(&st);
        }

        // Determine reply target: workflow engine tasks route back to sm:<id>.
        let reply_target =
            if let Some(sm_id) = msg.payload.get("sm_instance_id").and_then(|v| v.as_str()) {
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

        // Stream progress blocks to the bus as they arrive.
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let writer_fwd = writer.clone();
        let name_owned = name.to_string();
        let reply_owned = reply_target.clone();
        let msg_id_owned = msg.id.clone();
        let fwd_chat_id = telegram_chat_id;
        let fwd_task = tokio::spawn(async move {
            let mut full_response = String::new();
            let mut typing_stopped = false;
            while let Some(text) = progress_rx.recv().await {
                // On first output chunk, stop typing indicator and progress message.
                if !typing_stopped {
                    if let Some(chat_id) = fwd_chat_id {
                        let ctrl_target = format!("telegram.ctrl:{}", chat_id);
                        write_bus_envelope(
                            &writer_fwd,
                            &name_owned,
                            &ctrl_target,
                            serde_json::json!({"progress_done": true}),
                        )
                        .await;
                        write_bus_envelope(
                            &writer_fwd,
                            &name_owned,
                            &ctrl_target,
                            serde_json::json!({"typing": false}),
                        )
                        .await;
                    }
                    typing_stopped = true;
                }
                full_response.push_str(&text);
                // Skip forwarding empty/whitespace-only chunks to avoid
                // "(no content)" messages reaching Telegram.
                if !text.trim().is_empty() {
                    write_bus_envelope(
                        &writer_fwd,
                        &name_owned,
                        &reply_owned,
                        serde_json::json!({"result": text, "in_reply_to": msg_id_owned}),
                    )
                    .await;
                }
            }
            full_response
        });

        let image = image_base64.as_deref().zip(image_media_type.as_deref());

        let task_start = std::time::Instant::now();

        // Use the persistent process. send_task writes to its stdin and reads
        // stdout events until the result event marks the turn complete.
        let mut task_fut = Box::pin(process.send_task(task, Some(&progress_tx), image, &limits));

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
                            let _ = process.inject_message(inject_task);
                            // Restart typing indicator for the new message
                            if let Some(chat_id) = telegram_chat_id {
                                let ctrl_target = format!("telegram.ctrl:{}", chat_id);
                                write_bus_envelope(
                                    &writer,
                                    name,
                                    &ctrl_target,
                                    serde_json::json!({"typing": true}),
                                )
                                .await;
                                write_bus_envelope(
                                    &writer,
                                    name,
                                    &ctrl_target,
                                    serde_json::json!({"progress_start": true}),
                                )
                                .await;
                            }
                        }
                    } else {
                        warn!(agent = %name, "invalid message from bus during task, skipping");
                    }
                }
            }
        };

        // Drop the future to release borrows on process and progress_tx.
        drop(task_fut);
        drop(progress_tx);
        let full_response = fwd_task.await.unwrap_or_default();

        // Cancel progress timer if running.
        if let Some(cancel_tx) = progress_cancel_tx {
            let _ = cancel_tx.send(());
        }

        // Always send typing stop for Telegram tasks — even if already stopped
        // in the fwd_task on first chunk, this ensures cleanup on error/empty responses.
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

        set_idle(name);
        let task_duration = task_start.elapsed().as_secs();
        let task_duration_ms = task_start.elapsed().as_millis() as u64;
        match result {
            Ok(ref turn) => {
                info!(
                    agent = %name,
                    cost = turn.cost_usd,
                    turns = turn.num_turns,
                    "task completed (persistent)"
                );

                // Log token usage to JSONL file.
                log_token_usage(
                    &initial_state.config.work_dir,
                    name,
                    &msg.source,
                    task,
                    &turn.token_usage,
                    task_duration,
                    &initial_state.config.model,
                );

                // Log task completion.
                let log_entry = tasklog::TaskLog {
                    ts: chrono::Utc::now().to_rfc3339(),
                    source: msg.source.clone(),
                    turns: turn.num_turns,
                    cost: turn.cost_usd,
                    duration_ms: task_duration_ms,
                    status: "ok".to_string(),
                    task: tasklog::truncate_task(task_raw, 60),
                    error: None,
                    msg_id: msg.id.clone(),
                };
                if let Err(e) = tasklog::log_task(name, &log_entry) {
                    warn!(agent = %name, error = %e, "failed to write task log");
                }

                // Write to file-based inbox for all senders.
                let response = if full_response.is_empty() {
                    turn.response_text.clone()
                } else {
                    full_response
                };
                if !response.is_empty() {
                    write_inbox(name, &msg, task, Some(response), None);
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
                let err_str = format!("{}", e);
                warn!(agent = %name, error = %err_str, "task failed");

                // Log task failure.
                let log_entry = tasklog::TaskLog {
                    ts: chrono::Utc::now().to_rfc3339(),
                    source: msg.source.clone(),
                    turns: 0,
                    cost: 0.0,
                    duration_ms: task_duration_ms,
                    status: "error".to_string(),
                    task: tasklog::truncate_task(task_raw, 60),
                    error: Some(err_str.clone()),
                    msg_id: msg.id.clone(),
                };
                if let Err(le) = tasklog::log_task(name, &log_entry) {
                    warn!(agent = %name, error = %le, "failed to write task log");
                }

                // If the persistent process died, try to restart it.
                if err_str.contains("process exited") || err_str.contains("stdin closed") {
                    warn!(agent = %name, "agent process crashed, restarting");
                    match RuntimeProcess::start(name, &effective_bus, &agent_runtime).await {
                        Ok(new_proc) => {
                            process = new_proc;
                            info!(agent = %name, "agent process restarted");
                        }
                        Err(re) => {
                            warn!(agent = %name, error = %re, "failed to restart agent process");
                        }
                    }
                }

                write_inbox(name, &msg, task, None, Some(err_str.clone()));
                write_bus_envelope(
                    &writer,
                    name,
                    &reply_target,
                    serde_json::json!({"error": err_str, "in_reply_to": msg.id}),
                )
                .await;
            }
        }
    }

    process.stop().await;
    info!(agent = %name, "disconnected from bus");
    Ok(())
}

/// Log token usage for a completed task to a JSONL file.
fn log_token_usage(
    work_dir: &str,
    agent_name: &str,
    source: &str,
    task: &str,
    usage: &TokenUsage,
    duration_secs: u64,
    model: &str,
) {
    let deskd_dir = std::path::Path::new(work_dir).join(".deskd");
    if let Err(e) = std::fs::create_dir_all(&deskd_dir) {
        warn!(agent = %agent_name, error = %e, "failed to create .deskd dir for usage log");
        return;
    }

    let usage_path = deskd_dir.join("usage.jsonl");
    let truncated_task = if task.len() > 80 {
        let mut end = 80;
        while end > 0 && !task.is_char_boundary(end) {
            end -= 1;
        }
        &task[..end]
    } else {
        task
    };

    let entry = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "agent": agent_name,
        "source": source,
        "task": truncated_task,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read": usage.cache_read_input_tokens,
        "cache_creation": usage.cache_creation_input_tokens,
        "duration_secs": duration_secs,
        "model": model,
    });

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&usage_path)
    {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{}", entry) {
                warn!(agent = %agent_name, error = %e, "failed to write usage log");
            }
        }
        Err(e) => {
            warn!(agent = %agent_name, error = %e, "failed to open usage log");
        }
    }

    info!(
        agent = %agent_name,
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        cache_read = usage.cache_read_input_tokens,
        cache_creation = usage.cache_creation_input_tokens,
        duration_secs = duration_secs,
        "token usage"
    );
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
