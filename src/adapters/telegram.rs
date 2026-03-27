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
        Box::pin(run(
            self.token,
            bus_socket,
            agent_name,
            allowed_chats,
            mention_only,
            chat_names,
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
pub async fn run(
    token: String,
    socket_path: String,
    agent_name: String,
    allowed_chats: std::collections::HashSet<i64>,
    mention_only_chats: Vec<i64>,
    chat_names: std::collections::HashMap<i64, String>,
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
                    let _ = bot
                        .edit_message_text(ChatId(chat_id), msg_id, text)
                        .await;
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
async fn polling_loop(
    bot: Bot,
    socket_path: String,
    agent_name: String,
    bot_username: String,
    allowed_chats: std::collections::HashSet<i64>,
    mention_only_chats: std::collections::HashSet<i64>,
    chat_names: std::collections::HashMap<i64, String>,
) -> Result<()> {
    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let socket = socket_path.clone();
        let agent = agent_name.clone();
        let bot_user = bot_username.clone();
        let allowed = allowed_chats.clone();
        let mention_only = mention_only_chats.clone();
        let names = chat_names.clone();
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

                let target = format!("telegram.in:{}", chat_id);
                let reply_to = format!("telegram.out:{}", chat_id);
                let chat_name = names.get(&chat_id).cloned();

                debug!(agent = %agent, chat_id = chat_id, "received Telegram message");

                if let Err(e) =
                    publish_to_bus(&socket, &agent, &text, &target, &reply_to, chat_id, chat_name, image_base64.as_deref())
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
    });

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

/// Convert a subset of Markdown to Telegram HTML.
/// Handles: **bold**, *italic*, `inline code`, ```code blocks```.
/// Escapes &, <, > in plain text portions.
pub fn markdown_to_html(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        // Code block: ```...```
        if ch == '`' && chars.peek() == Some(&'`') {
            let mut backticks = 1usize;
            while chars.peek() == Some(&'`') {
                chars.next();
                backticks += 1;
            }
            if backticks >= 3 {
                // Consume optional language hint on the same line
                let mut code = String::new();
                let mut first_newline = true;
                while let Some(c) = chars.next() {
                    if first_newline && c != '\n' {
                        continue; // skip language hint
                    }
                    first_newline = false;
                    // Check for closing ```
                    if c == '`' {
                        let mut close = 1usize;
                        while chars.peek() == Some(&'`') {
                            chars.next();
                            close += 1;
                        }
                        if close >= 3 {
                            break;
                        }
                        code.push_str(&"`".repeat(close));
                        continue;
                    }
                    code.push(c);
                }
                result.push_str("<pre>");
                result.push_str(&html_escape(code.trim()));
                result.push_str("</pre>");
                continue;
            }
            // Not a triple backtick — treat as inline code
            result.push('`');
            continue;
        }

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

        // Bold: **...**
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume second *
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

        // Plain text — escape HTML special chars
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
