/// Telegram adapter — polls Telegram Bot API and bridges messages to/from the agent bus.
///
/// Channel naming (per issue #20):
///   Incoming:  adapter publishes `telegram.in:<chat_id>`
///   Outgoing:  adapter subscribes to `telegram.out:<chat_id>` (glob: `telegram.out:*`)
///
/// The adapter ignores messages from the bot itself to prevent reply loops.
/// Outbound text is converted from Markdown to Telegram HTML before sending.
use anyhow::{Context, Result};
use serde_json::Value;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Run the Telegram adapter for a specific agent.
/// `agent_name` is used to name the bus registration and for logging.
pub async fn run(token: String, socket_path: String, agent_name: String) -> Result<()> {
    info!(agent = %agent_name, "starting Telegram adapter");

    let bot = Bot::new(token);
    let bot_username = bot
        .get_me()
        .await
        .map(|me| me.username().to_string())
        .unwrap_or_default();

    info!(agent = %agent_name, bot = %bot_username, "Telegram bot connected");

    let adapter_name = format!("telegram-{}", agent_name);

    // Channel for outbound messages: (chat_id, text)
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<(i64, String)>();

    // Task 1: connect to bus, subscribe to telegram.out:*, forward to outbound channel
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

    // Task 2: send outbound messages to Telegram
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
        tokio::spawn(async move {
            if let Err(e) = polling_loop(bot, socket, name, bot_username).await {
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

/// Subscribe to `telegram.out:*` on the bus and forward messages to the outbound channel.
async fn bus_loop(
    socket_path: &str,
    adapter_name: &str,
    outbound_tx: mpsc::UnboundedSender<(i64, String)>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("telegram adapter: failed to connect to bus at {}", socket_path))?;

    let reg = serde_json::json!({
        "type": "register",
        "name": adapter_name,
        "subscriptions": ["telegram.out:*"],
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    info!(adapter = %adapter_name, "telegram adapter registered on bus");

    let (reader, _) = stream.into_split();
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

        // Target format: "telegram.out:<chat_id>"
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
                .and_then(|p| p.get("result").or_else(|| p.get("task")).or_else(|| p.get("error")))
                .and_then(|t| t.as_str())
                .unwrap_or("(no content)");

            debug!(chat_id = chat_id, "forwarding bus message to Telegram");
            if outbound_tx.send((chat_id, text.to_string())).is_err() {
                warn!("telegram adapter: outbound channel closed");
                break;
            }
        }
    }

    Ok(())
}

/// Send messages from the outbound channel to Telegram.
/// Converts Markdown to HTML; falls back to plain text if HTML parse fails.
async fn outbound_sender(bot: Bot, mut rx: mpsc::UnboundedReceiver<(i64, String)>) {
    while let Some((chat_id, text)) = rx.recv().await {
        let chat = ChatId(chat_id);
        let html = markdown_to_html(&text);

        // Try HTML first, fall back to plain text on error.
        let result = bot
            .send_message(chat, &html)
            .parse_mode(ParseMode::Html)
            .await;

        if result.is_err() {
            if let Err(e) = bot.send_message(chat, &text).await {
                warn!(chat_id = chat_id, error = %e, "failed to send Telegram message");
            }
        }
    }
}

/// Poll Telegram for incoming messages and publish them to the bus as `telegram.in:<chat_id>`.
async fn polling_loop(
    bot: Bot,
    socket_path: String,
    agent_name: String,
    bot_username: String,
) -> Result<()> {
    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let socket = socket_path.clone();
        let agent = agent_name.clone();
        let bot_user = bot_username.clone();
        async move {
            // Skip messages from the bot itself to prevent reply loops.
            if msg.from().map(|u| u.username.as_deref() == Some(&bot_user)).unwrap_or(false) {
                return Ok(());
            }
            // Skip messages from other bots.
            if msg.from().map(|u| u.is_bot).unwrap_or(false) {
                return Ok(());
            }

            if let Some(text) = msg.text() {
                let chat_id = msg.chat.id.0;
                let target = format!("telegram.in:{}", chat_id);
                let reply_to = format!("telegram.out:{}", chat_id);

                debug!(agent = %agent, chat_id = chat_id, "received Telegram message");

                if let Err(e) = publish_to_bus(&socket, &agent, text, &target, &reply_to).await {
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
async fn publish_to_bus(
    socket_path: &str,
    agent_name: &str,
    text: &str,
    target: &str,
    reply_to: &str,
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

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": format!("telegram-{}", agent_name),
        "target": target,
        "payload": {
            "task": text,
        },
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
        let id: i64 = target.strip_prefix("telegram.out:").unwrap().parse().unwrap();
        assert_eq!(id, -1001234567890i64);
    }
}
