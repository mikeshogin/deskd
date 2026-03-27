/// Telegram adapter — polls Telegram Bot API and bridges messages to/from the agent bus.
///
/// Channel naming (per issue #20):
///   Incoming:  adapter publishes `telegram.in:<chat_id>`
///   Outgoing:  adapter subscribes to `telegram.out:<chat_id>` (glob: `telegram.out:*`)
///
/// The adapter ignores messages from the bot itself to prevent reply loops.
/// Outbound text is converted from Markdown to Telegram HTML before sending.
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ParseMode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::TelegramRoute;

/// Telegram sender metadata passed through the bus payload.
struct SenderInfo {
    id: u64,
    username: Option<String>,
    first_name: String,
    is_bot: bool,
}

pub struct TelegramAdapter {
    token: String,
    routes: Vec<TelegramRoute>,
}

impl TelegramAdapter {
    pub fn new(token: String, routes: Vec<TelegramRoute>) -> Self {
        Self { token, routes }
    }
}

impl super::Adapter for TelegramAdapter {
    fn name(&self) -> &str {
        "telegram"
    }

    fn run(self: Box<Self>, bus_socket: String, agent_name: String) -> super::BoxFuture {
        let allowed_chats = self.routes.iter().map(|r| r.chat_id).collect();
        let mention_only = self
            .routes
            .iter()
            .filter(|r| r.mention_only)
            .map(|r| r.chat_id)
            .collect();
        let chat_names = self
            .routes
            .iter()
            .filter_map(|r| r.name.as_ref().map(|n| (r.chat_id, n.clone())))
            .collect();
        let chat_route_to: std::collections::HashMap<i64, String> = self
            .routes
            .iter()
            .filter_map(|r| r.route_to.as_ref().map(|t| (r.chat_id, t.clone())))
            .collect();
        Box::pin(run(
            self.token,
            bus_socket,
            agent_name,
            allowed_chats,
            mention_only,
            chat_names,
            chat_route_to,
        ))
    }
}

enum OutboundCmd {
    Text { chat_id: i64, text: String },
    TypingStart(i64),
    TypingStop(i64),
    ProgressStart(i64),
    ProgressUpdate { chat_id: i64, text: String },
    ProgressDone(i64),
}

/// Run the Telegram adapter for a specific agent.
/// `agent_name` is used to name the bus registration and for logging.
/// `allowed_chats` is the whitelist of chat_ids to accept messages from — all others are ignored.
/// `mention_only_chats` is a subset where only @mentions trigger the agent.
/// `chat_names` maps chat_id to a human-readable name shown to the agent as context.
/// `chat_route_to` maps chat_id to a bus target override (e.g. "agent:collab").
pub async fn run(
    token: String,
    socket_path: String,
    agent_name: String,
    allowed_chats: std::collections::HashSet<i64>,
    mention_only_chats: Vec<i64>,
    chat_names: std::collections::HashMap<i64, String>,
    chat_route_to: std::collections::HashMap<i64, String>,
) -> Result<()> {
    info!(agent = %agent_name, "starting Telegram adapter");

    let bot = Bot::new(token);
    let bot_username = bot
        .get_me()
        .await
        .map(|me| me.username().to_string())
        .unwrap_or_default();

    info!(agent = %agent_name, bot = %bot_username, "Telegram bot connected");

    let adapter_name = format!("telegram-{}", agent_name);

    // Channel for outbound commands: text messages + typing control
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundCmd>();

    // Task 1: connect to bus, subscribe to telegram.out:* and telegram.ctrl:*, forward to outbound channel
    let bus_task = {
        let outbound_tx = outbound_tx.clone();
        let socket = socket_path.clone();
        let name = adapter_name.clone();
        tokio::spawn(async move {
            if let Err(e) = bus_loop(&socket, &name, outbound_tx).await {
                tracing::error!(error = %e, "telegram bus loop failed");
            }
        })
    };

    // Task 2: send outbound messages to Telegram, manage typing indicators
    let sender_task = {
        let bot = bot.clone();
        tokio::spawn(async move {
            outbound_sender(bot, outbound_rx).await;
        })
    };

    // Task 3: poll Telegram for incoming messages and publish telegram.in:<chat_id>
    let polling_task = {
        let socket = socket_path.clone();
        let name = agent_name.clone();
        let mention_only: std::collections::HashSet<i64> = mention_only_chats.into_iter().collect();
        tokio::spawn(async move {
            if let Err(e) = polling_loop(
                bot,
                socket,
                name,
                bot_username,
                allowed_chats,
                mention_only,
                chat_names,
                chat_route_to,
            )
            .await
            {
                tracing::error!(error = %e, "telegram polling loop failed");
            }
        })
    };

    tokio::select! {
        _ = bus_task => warn!(agent = %agent_name, "telegram bus task exited"),
        _ = sender_task => warn!(agent = %agent_name, "telegram sender task exited"),
        _ = polling_task => warn!(agent = %agent_name, "telegram polling task exited"),
    }

    Ok(())
}

/// Subscribe to `telegram.out:*` and `telegram.ctrl:*` on the bus, forward to outbound channel.
async fn bus_loop(
    socket_path: &str,
    adapter_name: &str,
    outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "telegram adapter: failed to connect to bus at {}",
            socket_path
        )
    })?;

    let reg = serde_json::json!({
        "type": "register",
        "name": adapter_name,
        "subscriptions": ["telegram.out:*", "telegram.ctrl:*"],
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    info!(adapter = %adapter_name, "telegram adapter registered on bus");

    // Keep _writer alive — dropping it closes the connection.
    let (reader, _writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "telegram adapter: invalid message from bus");
                continue;
            }
        };

        let target = msg.get("target").and_then(|t| t.as_str()).unwrap_or("");

        // Target format: "telegram.out:<chat_id>" — send text message
        if let Some(chat_id_str) = target.strip_prefix("telegram.out:") {
            let chat_id: i64 = match chat_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    warn!(target = %target, "telegram adapter: invalid chat_id in target");
                    continue;
                }
            };

            let text = msg
                .get("payload")
                .and_then(|p| {
                    p.get("result")
                        .or_else(|| p.get("task"))
                        .or_else(|| p.get("error"))
                })
                .and_then(|t| t.as_str())
                .unwrap_or("(no content)");

            debug!(chat_id = chat_id, "forwarding bus message to Telegram");
            if outbound_tx
                .send(OutboundCmd::Text {
                    chat_id,
                    text: text.to_string(),
                })
                .is_err()
            {
                warn!("telegram adapter: outbound channel closed");
                break;
            }

        // Target format: "telegram.ctrl:<chat_id>" — typing control
        } else if let Some(chat_id_str) = target.strip_prefix("telegram.ctrl:") {
            let chat_id: i64 = match chat_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    warn!(target = %target, "telegram adapter: invalid chat_id in ctrl target");
                    continue;
                }
            };

            let payload = msg.get("payload");

            let cmd = if payload
                .and_then(|p| p.get("progress_start"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                OutboundCmd::ProgressStart(chat_id)
            } else if let Some(text) = payload
                .and_then(|p| p.get("progress_update"))
                .and_then(|v| v.as_str())
            {
                OutboundCmd::ProgressUpdate {
                    chat_id,
                    text: text.to_string(),
                }
            } else if payload
                .and_then(|p| p.get("progress_done"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                OutboundCmd::ProgressDone(chat_id)
            } else {
                let typing = payload
                    .and_then(|p| p.get("typing"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if typing {
                    OutboundCmd::TypingStart(chat_id)
                } else {
                    OutboundCmd::TypingStop(chat_id)
                }
            };

            if outbound_tx.send(cmd).is_err() {
                warn!("telegram adapter: outbound channel closed");
                break;
            }
        }
    }

    Ok(())
}

/// Spawn a background loop that sends typing action every 4 seconds.
/// Telegram shows the indicator for ~5s, so 4s keeps it alive continuously.
fn spawn_typing(bot: Bot, chat_id: i64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let _ = bot
                .send_chat_action(ChatId(chat_id), ChatAction::Typing)
                .await;
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    })
}

/// Send messages from the outbound channel to Telegram.
/// Manages per-chat typing indicators, progress messages, and converts Markdown to HTML.
async fn outbound_sender(bot: Bot, mut rx: mpsc::UnboundedReceiver<OutboundCmd>) {
    let mut typing_tasks: HashMap<i64, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut progress_ids: HashMap<i64, teloxide::types::MessageId> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            OutboundCmd::Text { chat_id, text } => {
                // Abort current typing loop while sending so Telegram doesn't show
                // typing and a new message at the same time.
                if let Some(handle) = typing_tasks.remove(&chat_id) {
                    handle.abort();
                }

                let chat = ChatId(chat_id);
                let html = markdown_to_html(&text);

                // Try HTML first, fall back to plain text on error.
                let result = bot
                    .send_message(chat, &html)
                    .parse_mode(ParseMode::Html)
                    .await;

                if result.is_err()
                    && let Err(e) = bot.send_message(chat, &text).await
                {
                    warn!(chat_id = chat_id, error = %e, "failed to send Telegram message");
                }

                // Restart typing loop — more streaming chunks may still be coming.
                // Will be cancelled by TypingStop when the task finishes.
                typing_tasks.insert(chat_id, spawn_typing(bot.clone(), chat_id));
            }
            OutboundCmd::TypingStart(chat_id) => {
                // Cancel any existing typing task for this chat.
                if let Some(handle) = typing_tasks.remove(&chat_id) {
                    handle.abort();
                }
                typing_tasks.insert(chat_id, spawn_typing(bot.clone(), chat_id));
            }
            OutboundCmd::TypingStop(chat_id) => {
                if let Some(handle) = typing_tasks.remove(&chat_id) {
                    handle.abort();
                }
            }
            OutboundCmd::ProgressStart(chat_id) => {
                if let Ok(msg) = bot.send_message(ChatId(chat_id), "⏳ Working...").await {
                    progress_ids.insert(chat_id, msg.id);
                }
            }
            OutboundCmd::ProgressUpdate { chat_id, text } => {
                if let Some(&msg_id) = progress_ids.get(&chat_id) {
                    let _ = bot.edit_message_text(ChatId(chat_id), msg_id, text).await;
                }
            }
            OutboundCmd::ProgressDone(chat_id) => {
                if let Some(msg_id) = progress_ids.remove(&chat_id) {
                    let _ = bot.delete_message(ChatId(chat_id), msg_id).await;
                }
            }
        }
    }
}

/// Download a Telegram photo into memory and return its base64-encoded bytes.
/// Takes the largest available size (last element in the PhotoSize array).
async fn download_photo_base64(bot: &Bot, file_id: &str) -> Result<String> {
    let tg_file = bot
        .get_file(file_id)
        .await
        .with_context(|| format!("failed to get file info for {}", file_id))?;

    let mut buf: Vec<u8> = Vec::new();
    bot.download_file(&tg_file.path, &mut buf)
        .await
        .with_context(|| format!("failed to download file {}", file_id))?;

    Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
}

/// Poll Telegram for incoming messages and publish them to the bus as `telegram.in:<chat_id>`.
#[allow(clippy::too_many_arguments)]
async fn polling_loop(
    bot: Bot,
    socket_path: String,
    agent_name: String,
    bot_username: String,
    allowed_chats: std::collections::HashSet<i64>,
    mention_only_chats: std::collections::HashSet<i64>,
    chat_names: std::collections::HashMap<i64, String>,
    chat_route_to: std::collections::HashMap<i64, String>,
) -> Result<()> {
    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let socket = socket_path.clone();
        let agent = agent_name.clone();
        let bot_user = bot_username.clone();
        let allowed = allowed_chats.clone();
        let mention_only = mention_only_chats.clone();
        let names = chat_names.clone();
        let route_to_map = chat_route_to.clone();
        async move {
            // Skip messages from the bot itself to prevent reply loops.
            if msg.from.as_ref().map(|u| u.username.as_deref() == Some(&bot_user)).unwrap_or(false) {
                return Ok(());
            }
            // Skip messages from other bots.
            if msg.from.as_ref().map(|u| u.is_bot).unwrap_or(false) {
                return Ok(());
            }

            // Determine the task text and optional image data from the message.
            // Photos are base64-encoded in memory and passed alongside the caption.
            // Pure text messages are passed through unchanged.
            let mut image_base64: Option<String> = None;
            let task_text: Option<String> = if let Some(photos) = msg.photo() {
                // Take the largest photo (Telegram sends sizes smallest → largest).
                if let Some(largest) = photos.last() {
                    let file_id = largest.file.id.clone();
                    match download_photo_base64(&bot, &file_id).await {
                        Ok(b64) => {
                            image_base64 = Some(b64);
                            let caption = msg.caption().unwrap_or("[photo attached]");
                            Some(caption.to_string())
                        }
                        Err(e) => {
                            warn!(file_id = %file_id, error = %e, "failed to download Telegram photo");
                            // Fall back to caption-only so the message isn't silently dropped.
                            let caption = msg.caption().unwrap_or("[photo attached — download failed]");
                            Some(caption.to_string())
                        }
                    }
                } else {
                    None
                }
            } else {
                msg.text().map(|t| t.to_string())
            };

            if let Some(text) = task_text {
                let chat_id = msg.chat.id.0;

                // Whitelist check — only process chats explicitly configured in routes.
                if !allowed.is_empty() && !allowed.contains(&chat_id) {
                    debug!(agent = %agent, chat_id = chat_id, "ignoring message — chat not in whitelist");
                    return Ok(());
                }

                // If this chat requires a mention, skip unless @bot_user appears in text.
                if mention_only.contains(&chat_id) {
                    let mention = format!("@{}", bot_user);
                    if !text.contains(&mention) {
                        debug!(agent = %agent, chat_id = chat_id, "skipping message — not a mention");
                        return Ok(());
                    }
                }

                // Use route_to override if configured, otherwise default to telegram.in:<chat_id>.
                let target = if let Some(rt) = route_to_map.get(&chat_id) {
                    rt.clone()
                } else {
                    format!("telegram.in:{}", chat_id)
                };
                // reply_to always goes back to Telegram so agent responses reach the chat.
                let reply_to = format!("telegram.out:{}", chat_id);
                let chat_name = names.get(&chat_id).cloned();

                debug!(agent = %agent, chat_id = chat_id, "received Telegram message");

                // Extract sender metadata from the Telegram message.
                let sender_info = msg.from.as_ref().map(|u| SenderInfo {
                    id: u.id.0,
                    username: u.username.clone(),
                    first_name: u.first_name.clone(),
                    is_bot: u.is_bot,
                });
                let message_id = msg.id.0;
                let reply_to_message_id = msg.reply_to_message().map(|r| r.id.0);

                if let Err(e) =
                    publish_to_bus(&socket, &agent, &text, &target, &reply_to, chat_id, chat_name, image_base64.as_deref(), sender_info.as_ref(), message_id, reply_to_message_id)
                        .await
                {
                    warn!(chat_id = chat_id, error = %e, "failed to publish message to bus");
                    let _ = bot
                        .send_message(msg.chat.id, "Internal error, please try again.")
                        .await;
                }
            }
            Ok(())
        }
    })
    .await;

    Ok(())
}

/// Publish an incoming Telegram message to the bus as `telegram.in:<chat_id>`.
/// When `image_base64` is provided, the payload includes `image_base64` and `image_media_type`
/// so the worker can send a multimodal message to Claude via stream-json.
#[allow(clippy::too_many_arguments)]
async fn publish_to_bus(
    socket_path: &str,
    agent_name: &str,
    text: &str,
    target: &str,
    reply_to: &str,
    chat_id: i64,
    chat_name: Option<String>,
    image_base64: Option<&str>,
    sender_info: Option<&SenderInfo>,
    message_id: i32,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to bus at {}", socket_path))?;

    let reg = serde_json::json!({
        "type": "register",
        "name": format!("telegram-in-{}", Uuid::new_v4()),
        "subscriptions": [],
    });
    let mut reg_line = serde_json::to_string(&reg)?;
    reg_line.push('\n');
    stream.write_all(reg_line.as_bytes()).await?;

    let mut payload = serde_json::json!({
        "task": text,
        "telegram_chat_id": chat_id,
        "telegram_chat_name": chat_name,
        "telegram_message_id": message_id,
    });

    if let Some(reply_id) = reply_to_message_id {
        payload["telegram_reply_to_message_id"] = serde_json::json!(reply_id);
    }

    if let Some(sender) = sender_info {
        payload["telegram_sender"] = serde_json::json!({
            "id": sender.id,
            "username": sender.username,
            "first_name": sender.first_name,
            "is_bot": sender.is_bot,
        });
    }

    if let Some(b64) = image_base64 {
        payload["image_base64"] = serde_json::Value::String(b64.to_string());
        payload["image_media_type"] = serde_json::Value::String("image/jpeg".to_string());
    }

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": format!("telegram-{}", agent_name),
        "target": target,
        "payload": payload,
        "reply_to": reply_to,
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    Ok(())
}

// ─── Markdown → Telegram HTML ─────────────────────────────────────────────────

/// Convert Markdown to Telegram HTML.
///
/// Block-level (processed line by line):
///   `# / ## / ###`    → bold with ▶ / ▸ / • prefix
///   `--- / *** / ___` → ───────────────────── separator
///   `> blockquote`    → `<blockquote>`
///   `| table |`       → ASCII table inside `<pre>`
///   ` ``` code ``` `  → `<pre>`
///
/// Inline (within each line):
///   **bold**, *italic*, ~~strikethrough~~, `code`
pub fn markdown_to_html(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        // ── Fenced code block ──────────────────────────────────────────────
        if line.trim_start().starts_with("```") {
            let mut code = String::new();
            for inner in lines.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
                code.push_str(inner);
                code.push('\n');
            }
            result.push_str("<pre>");
            result.push_str(&html_escape(code.trim_end()));
            result.push_str("</pre>\n");
            continue;
        }

        // ── Horizontal rule ────────────────────────────────────────────────
        let trimmed = line.trim();
        if (trimmed == "---" || trimmed == "***" || trimmed == "___")
            || (trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-'))
        {
            result.push_str("─────────────────────\n");
            continue;
        }

        // ── ATX headers (# / ## / ###) ─────────────────────────────────────
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if hashes > 0 && hashes <= 6 {
            let rest = trimmed[hashes..].trim_start();
            if !rest.is_empty() {
                let prefix = match hashes {
                    1 => "▶ ",
                    2 => "▸ ",
                    _ => "• ",
                };
                result.push_str("<b>");
                result.push_str(prefix);
                result.push_str(&inline_to_html(rest));
                result.push_str("</b>\n");
                continue;
            }
        }

        // ── Table: collect consecutive pipe-containing lines ───────────────
        if trimmed.starts_with('|') || (trimmed.contains(" | ") && trimmed.contains('|')) {
            let mut table_lines: Vec<&str> = vec![line];
            while let Some(&next) = lines.peek() {
                let nt = next.trim();
                if nt.starts_with('|') || (nt.contains(" | ") && nt.contains('|')) {
                    table_lines.push(lines.next().unwrap());
                } else {
                    break;
                }
            }
            if table_lines.len() >= 2 {
                result.push_str(&render_table(&table_lines));
                result.push('\n');
                continue;
            }
        }

        // ── Blockquote ─────────────────────────────────────────────────────
        if let Some(inner) = trimmed
            .strip_prefix("> ")
            .or_else(|| if trimmed == ">" { Some("") } else { None })
        {
            result.push_str("<blockquote>");
            result.push_str(&inline_to_html(inner));
            result.push_str("</blockquote>\n");
            continue;
        }

        // ── Normal line ────────────────────────────────────────────────────
        result.push_str(&inline_to_html(line));
        result.push('\n');
    }

    result.trim_end().to_string()
}

/// Render a markdown table as an ASCII table inside <pre>.
/// Skips separator rows (---|---).
fn render_table(rows: &[&str]) -> String {
    let parsed: Vec<Vec<String>> = rows
        .iter()
        .filter(|row| !is_table_separator(row))
        .map(|row| {
            let trimmed = row.trim().trim_matches('|');
            trimmed
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect();

    if parsed.is_empty() {
        return String::new();
    }

    let num_cols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; num_cols];
    for row in &parsed {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let mut out = String::from("<pre>");
    for (idx, row) in parsed.iter().enumerate() {
        let cells: Vec<String> = (0..num_cols)
            .map(|i| {
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                format!("{:<width$}", cell, width = widths[i])
            })
            .collect();
        out.push_str(&html_escape(&cells.join(" │ ")));
        out.push('\n');
        // Separator after header row
        if idx == 0 && parsed.len() > 1 {
            let sep: String = widths
                .iter()
                .map(|&w| "─".repeat(w))
                .collect::<Vec<_>>()
                .join("─┼─");
            out.push_str(&html_escape(&sep));
            out.push('\n');
        }
    }
    out.push_str("</pre>");
    out
}

fn is_table_separator(row: &str) -> bool {
    row.trim().trim_matches('|').split('|').all(|cell| {
        cell.trim()
            .chars()
            .all(|c| c == '-' || c == ':' || c == ' ')
    }) && row.contains('-')
}

/// Apply inline Markdown formatting to a single line of text.
/// Handles: **bold**, *italic*, ~~strikethrough~~, `code`.
fn inline_to_html(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        // Inline code: `...`
        if ch == '`' {
            let mut code = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '`' {
                    closed = true;
                    break;
                }
                code.push(c);
            }
            if closed {
                result.push_str("<code>");
                result.push_str(&html_escape(&code));
                result.push_str("</code>");
            } else {
                result.push('`');
                result.push_str(&html_escape(&code));
            }
            continue;
        }

        // Strikethrough: ~~...~~
        if ch == '~' && chars.peek() == Some(&'~') {
            chars.next();
            let mut inner = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '~' && chars.peek() == Some(&'~') {
                    chars.next();
                    closed = true;
                    break;
                }
                inner.push(c);
            }
            if closed {
                result.push_str("<s>");
                result.push_str(&html_escape(&inner));
                result.push_str("</s>");
            } else {
                result.push_str("~~");
                result.push_str(&html_escape(&inner));
            }
            continue;
        }

        // Bold: **...**
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next();
            let mut inner = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    closed = true;
                    break;
                }
                inner.push(c);
            }
            if closed {
                result.push_str("<b>");
                result.push_str(&html_escape(&inner));
                result.push_str("</b>");
            } else {
                result.push_str("**");
                result.push_str(&html_escape(&inner));
            }
            continue;
        }

        // Italic: *...*
        if ch == '*' {
            let mut inner = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '*' {
                    closed = true;
                    break;
                }
                inner.push(c);
            }
            if closed {
                result.push_str("<i>");
                result.push_str(&html_escape(&inner));
                result.push_str("</i>");
            } else {
                result.push('*');
                result.push_str(&html_escape(&inner));
            }
            continue;
        }

        result.push_str(&html_escape_char(ch));
    }

    result
}

fn html_escape(s: &str) -> String {
    s.chars().map(html_escape_char).collect()
}

fn html_escape_char(c: char) -> String {
    match c {
        '&' => "&amp;".to_string(),
        '<' => "&lt;".to_string(),
        '>' => "&gt;".to_string(),
        '"' => "&quot;".to_string(),
        _ => c.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_to_html_bold() {
        assert_eq!(markdown_to_html("**hello**"), "<b>hello</b>");
    }

    #[test]
    fn test_markdown_to_html_italic() {
        assert_eq!(markdown_to_html("*hello*"), "<i>hello</i>");
    }

    #[test]
    fn test_markdown_to_html_inline_code() {
        assert_eq!(markdown_to_html("`foo`"), "<code>foo</code>");
    }

    #[test]
    fn test_markdown_to_html_html_escape() {
        assert_eq!(markdown_to_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn test_markdown_to_html_mixed() {
        let input = "Fixed **issue #42** in `bus.rs`";
        let out = markdown_to_html(input);
        assert!(out.contains("<b>issue #42</b>"));
        assert!(out.contains("<code>bus.rs</code>"));
    }

    #[test]
    fn test_markdown_to_html_headers() {
        assert!(markdown_to_html("# Title").contains("<b>▶ Title</b>"));
        assert!(markdown_to_html("## Sub").contains("<b>▸ Sub</b>"));
        assert!(markdown_to_html("### Deep").contains("<b>• Deep</b>"));
    }

    #[test]
    fn test_markdown_to_html_horizontal_rule() {
        assert!(markdown_to_html("---").contains("─────"));
        assert!(markdown_to_html("***").contains("─────"));
    }

    #[test]
    fn test_markdown_to_html_strikethrough() {
        assert_eq!(markdown_to_html("~~gone~~"), "<s>gone</s>");
    }

    #[test]
    fn test_markdown_to_html_blockquote() {
        assert!(markdown_to_html("> note").contains("<blockquote>note</blockquote>"));
    }

    #[test]
    fn test_markdown_to_html_table() {
        let input = "| Name | Value |\n|------|-------|\n| foo  | bar   |";
        let out = markdown_to_html(input);
        assert!(out.contains("<pre>"));
        assert!(out.contains("Name"));
        assert!(out.contains("foo"));
        assert!(out.contains("─"));
    }

    #[test]
    fn test_markdown_to_html_code_block() {
        let input = "```\nlet x = 1;\n```";
        let out = markdown_to_html(input);
        assert!(out.contains("<pre>"));
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn test_chat_id_negative() {
        let target = "telegram.out:-1001234567890";
        let id: i64 = target
            .strip_prefix("telegram.out:")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(id, -1001234567890i64);
    }
}
