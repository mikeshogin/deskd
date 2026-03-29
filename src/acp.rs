//! ACP (Agent Client Protocol) client for deskd worker.
//!
//! Implements JSON-RPC 2.0 framing over stdin/stdout to communicate with
//! ACP-compatible agents (Gemini CLI, Goose, etc.).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::agent::{self, AgentConfig, TaskLimits, TurnResult, build_command};
use crate::config::SessionMode;

// ─── JSON-RPC 2.0 types ─────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request (client -> agent).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        }
    }

    /// Serialize to a newline-delimited JSON string.
    pub fn to_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// A JSON-RPC 2.0 response (agent -> client).
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
    /// For notifications — the method name.
    pub method: Option<String>,
    /// For notifications — the params.
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// Parse a line of newline-delimited JSON into a JsonRpcResponse.
pub fn parse_response(line: &str) -> Result<JsonRpcResponse> {
    serde_json::from_str(line).context("failed to parse JSON-RPC response")
}

/// Classify incoming JSON-RPC messages into three categories.
#[derive(Debug, PartialEq)]
pub enum MessageKind {
    /// Server request: has both `id` and `method` (e.g. `session/request_permission`).
    ServerRequest,
    /// Notification: has `method` but no `id` (e.g. `session/update`).
    Notification,
    /// Response to a client request: has `id` but no `method`.
    Response,
}

pub fn classify_message(resp: &JsonRpcResponse) -> MessageKind {
    match (resp.id.is_some(), resp.method.is_some()) {
        (true, true) => MessageKind::ServerRequest,
        (false, true) => MessageKind::Notification,
        _ => MessageKind::Response,
    }
}

/// Check if a parsed response is a notification (no id, has method).
#[cfg(test)]
pub fn is_notification(resp: &JsonRpcResponse) -> bool {
    resp.id.is_none() && resp.method.is_some()
}

// ─── ACP message helpers ─────────────────────────────────────────────────────

/// Build an `initialize` request with minimal capabilities.
pub fn build_initialize(id: u64) -> JsonRpcRequest {
    JsonRpcRequest::new(
        id,
        "initialize",
        Some(serde_json::json!({
            "capabilities": {
                "fs": {
                    "readTextFile": false,
                    "writeTextFile": false
                },
                "terminal": false
            },
            "clientInfo": {
                "name": "deskd",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
    )
}

/// Build a `session/new` request.
pub fn build_session_new(
    id: u64,
    cwd: &str,
    mcp_servers: Option<HashMap<String, serde_json::Value>>,
) -> JsonRpcRequest {
    let mut params = serde_json::json!({
        "cwd": cwd,
    });
    if let Some(servers) = mcp_servers {
        params["mcpServers"] = serde_json::to_value(servers).unwrap_or_default();
    }
    JsonRpcRequest::new(id, "session/new", Some(params))
}

/// Build a `session/load` request to resume an existing session.
pub fn build_session_load(id: u64, session_id: &str) -> JsonRpcRequest {
    JsonRpcRequest::new(
        id,
        "session/load",
        Some(serde_json::json!({
            "sessionId": session_id,
        })),
    )
}

/// Build a `session/prompt` request.
pub fn build_session_prompt(id: u64, session_id: &str, text: &str) -> JsonRpcRequest {
    JsonRpcRequest::new(
        id,
        "session/prompt",
        Some(serde_json::json!({
            "sessionId": session_id,
            "text": text,
        })),
    )
}

/// Build a `session/cancel` request.
pub fn build_session_cancel(id: u64, session_id: &str) -> JsonRpcRequest {
    JsonRpcRequest::new(
        id,
        "session/cancel",
        Some(serde_json::json!({
            "sessionId": session_id,
        })),
    )
}

/// Build a permission approval response for `session/request_permission`.
pub fn build_permission_approval(request_id: u64) -> String {
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "approved": true
        }
    });
    let mut line = serde_json::to_string(&resp).expect("json serialization cannot fail");
    line.push('\n');
    line
}

/// Extract text content from a `session/update` notification.
/// Returns accumulated text from assistant messages in the update.
pub fn extract_update_text(params: &serde_json::Value) -> Option<String> {
    // ACP session/update notifications contain messages array.
    // Look for assistant messages with text content.
    let messages = params.get("messages").and_then(|m| m.as_array())?;
    let mut text = String::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        if role != "assistant" {
            continue;
        }

        // Content can be a string or array of content blocks.
        if let Some(content_str) = msg.get("content").and_then(|c| c.as_str()) {
            text.push_str(content_str);
        } else if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
            for block in content_arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(t) = block.get("text").and_then(|t| t.as_str())
                {
                    text.push_str(t);
                }
            }
        }
    }

    if text.is_empty() { None } else { Some(text) }
}

/// Check if a session/update notification indicates the session is complete.
pub fn is_session_complete(params: &serde_json::Value) -> bool {
    params
        .get("status")
        .and_then(|s| s.as_str())
        .is_some_and(|s| s == "completed" || s == "done" || s == "ended")
}

/// Extract session ID from an ACP response (session/new or session/load result).
pub fn extract_session_id(result: &serde_json::Value) -> Option<String> {
    result
        .get("sessionId")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

// ─── AcpProcess: long-lived ACP agent process ───────────────────────────────

/// Events emitted by the ACP stdout reader task.
enum AcpEvent {
    /// Text content from a session/update notification.
    TextBlock(String),
    /// A JSON-RPC response to a request we sent.
    Response(JsonRpcResponse),
    /// A permission request from the agent.
    PermissionRequest(u64),
    /// Session completed.
    SessionComplete,
    /// Process exited (stdout closed).
    ProcessExited,
}

/// A long-lived ACP agent process that communicates via JSON-RPC 2.0.
pub struct AcpProcess {
    /// Send lines to the agent's stdin.
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Receive parsed ACP events from stdout.
    event_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<AcpEvent>>,
    /// Child process handle for shutdown.
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    /// Agent name.
    name: String,
    /// Current ACP session ID.
    session_id: tokio::sync::Mutex<Option<String>>,
    /// Next JSON-RPC request ID.
    next_id: tokio::sync::Mutex<u64>,
}

impl AcpProcess {
    /// Spawn an ACP agent process and perform the initialize handshake.
    pub async fn start(name: &str, bus_socket: &str) -> Result<Self> {
        Self::spawn_and_init(name, bus_socket, false).await
    }

    /// Spawn an ACP agent process with a fresh session (ignore stored session_id).
    pub async fn start_fresh(name: &str, bus_socket: &str) -> Result<Self> {
        Self::spawn_and_init(name, bus_socket, true).await
    }

    async fn spawn_and_init(name: &str, bus_socket: &str, fresh: bool) -> Result<Self> {
        let state = agent::load_state(name)?;

        // ACP processes don't use Claude-specific args; just spawn the command as-is.
        let bus_path = bus_socket.to_string();
        let config_path_str = state.config.config_path.clone().unwrap_or_default();
        let mut extra_env: Vec<(&str, &str)> =
            vec![("DESKD_AGENT_NAME", name), ("DESKD_BUS_SOCKET", &bus_path)];
        if !config_path_str.is_empty() {
            extra_env.push(("DESKD_AGENT_CONFIG", &config_path_str));
        }

        // No extra args for ACP — the command in config should be complete.
        let args: Vec<String> = Vec::new();
        let mut cmd = build_command(&state.config, &args, &extra_env);
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().context("Failed to spawn ACP agent process")?;

        let child_stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        // Drain stderr in background.
        let agent_name = name.to_string();
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = tokio::io::BufReader::new(stderr);
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut buf).await;
            if !buf.is_empty() {
                warn!(agent = %agent_name, stderr = %buf.trim(), "ACP process stderr");
            }
        });

        // Stdin writer task.
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut writer = child_stdin;
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Stdout reader task — parses JSON-RPC messages.
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<AcpEvent>();
        let agent_name2 = name.to_string();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let resp = match parse_response(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        debug!(agent = %agent_name2, error = %e, line = %line, "unparseable ACP output");
                        continue;
                    }
                };

                match classify_message(&resp) {
                    MessageKind::ServerRequest => {
                        // Server request: has both id and method.
                        // e.g. session/request_permission — the server asks us
                        // to approve something; we must reply with the same id.
                        let method = resp.method.as_deref().unwrap_or_default();
                        match method {
                            "session/request_permission" => {
                                if let Some(req_id) = resp.id
                                    && event_tx.send(AcpEvent::PermissionRequest(req_id)).is_err()
                                {
                                    break;
                                }
                            }
                            _ => {
                                debug!(agent = %agent_name2, method = %method, "unknown ACP server request");
                            }
                        }
                    }
                    MessageKind::Notification => {
                        // Notification: has method but no id.
                        let method = resp.method.as_deref().unwrap_or_default();
                        let params = resp.params.as_ref().cloned().unwrap_or_default();

                        match method {
                            "session/update" => {
                                if let Some(text) = extract_update_text(&params)
                                    && event_tx.send(AcpEvent::TextBlock(text)).is_err()
                                {
                                    break;
                                }
                                if is_session_complete(&params)
                                    && event_tx.send(AcpEvent::SessionComplete).is_err()
                                {
                                    break;
                                }
                            }
                            _ => {
                                debug!(agent = %agent_name2, method = %method, "unknown ACP notification");
                            }
                        }
                    }
                    MessageKind::Response => {
                        // Response to a request we sent: has id but no method.
                        if event_tx.send(AcpEvent::Response(resp)).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = event_tx.send(AcpEvent::ProcessExited);
            debug!(agent = %agent_name2, "ACP process stdout closed");
        });

        let process = Self {
            stdin_tx,
            event_rx: tokio::sync::Mutex::new(event_rx),
            child: tokio::sync::Mutex::new(Some(child)),
            name: name.to_string(),
            session_id: tokio::sync::Mutex::new(None),
            next_id: tokio::sync::Mutex::new(1),
        };

        // Perform initialize handshake.
        process.do_initialize().await?;

        // Create or load session.
        let session_id = if !fresh
            && state.config.session == SessionMode::Persistent
            && !state.session_id.is_empty()
        {
            process.do_session_load(&state.session_id).await?
        } else {
            process.do_session_new(&state.config).await?
        };
        *process.session_id.lock().await = Some(session_id);

        info!(agent = %name, "ACP process initialized");
        Ok(process)
    }

    /// Get the next request ID.
    async fn next_request_id(&self) -> u64 {
        let mut id = self.next_id.lock().await;
        let current = *id;
        *id += 1;
        current
    }

    /// Send a JSON-RPC request and wait for the response with matching ID.
    /// Processes notifications (permission requests, updates) while waiting.
    async fn send_request(
        &self,
        req: &JsonRpcRequest,
        progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<JsonRpcResponse> {
        let line = req.to_line()?;
        self.stdin_tx
            .send(line)
            .map_err(|_| anyhow::anyhow!("ACP process stdin closed"))?;

        let expected_id = req.id;
        let mut event_rx = self.event_rx.lock().await;

        loop {
            match event_rx.recv().await {
                Some(AcpEvent::Response(resp)) => {
                    if resp.id == Some(expected_id) {
                        return Ok(resp);
                    }
                    // Response for a different request — skip.
                    debug!(
                        agent = %self.name,
                        resp_id = ?resp.id,
                        expected = expected_id,
                        "unexpected response ID"
                    );
                }
                Some(AcpEvent::PermissionRequest(req_id)) => {
                    let approval = build_permission_approval(req_id);
                    let _ = self.stdin_tx.send(approval);
                }
                Some(AcpEvent::TextBlock(text)) => {
                    if let Some(tx) = progress_tx {
                        let _ = tx.send(text);
                    }
                }
                Some(AcpEvent::SessionComplete) => {
                    // Session completed while waiting for response — keep waiting.
                }
                Some(AcpEvent::ProcessExited) | None => {
                    bail!("ACP process exited while waiting for response");
                }
            }
        }
    }

    /// Perform the initialize handshake.
    async fn do_initialize(&self) -> Result<()> {
        let id = self.next_request_id().await;
        let req = build_initialize(id);
        let resp = self.send_request(&req, None).await?;

        if let Some(err) = resp.error {
            bail!("ACP initialize failed: {}", err);
        }

        debug!(agent = %self.name, "ACP initialize succeeded");
        Ok(())
    }

    /// Create a new session and return its ID.
    async fn do_session_new(&self, cfg: &AgentConfig) -> Result<String> {
        let id = self.next_request_id().await;
        let req = build_session_new(id, &cfg.work_dir, None);
        let resp = self.send_request(&req, None).await?;

        if let Some(err) = resp.error {
            bail!("ACP session/new failed: {}", err);
        }

        let result = resp.result.unwrap_or_default();
        extract_session_id(&result)
            .ok_or_else(|| anyhow::anyhow!("session/new response missing sessionId"))
    }

    /// Load an existing session and return its ID.
    async fn do_session_load(&self, session_id: &str) -> Result<String> {
        let id = self.next_request_id().await;
        let req = build_session_load(id, session_id);
        let resp = self.send_request(&req, None).await?;

        if let Some(err) = resp.error {
            // If session load fails, fall back to creating a new session.
            warn!(
                agent = %self.name,
                session_id = %session_id,
                error = %err,
                "session/load failed, creating new session"
            );
            let state = agent::load_state(&self.name)?;
            return self.do_session_new(&state.config).await;
        }

        let result = resp.result.unwrap_or_default();
        extract_session_id(&result).ok_or_else(|| {
            anyhow::anyhow!("session/load response missing sessionId, using provided")
        })
    }

    /// Send a task to the ACP agent and collect the streamed response.
    pub async fn send_task(
        &self,
        message: &str,
        progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
        limits: &TaskLimits,
    ) -> Result<TurnResult> {
        let session_id = self
            .session_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no ACP session established"))?;

        let id = self.next_request_id().await;
        let req = build_session_prompt(id, &session_id, message);
        let line = req.to_line()?;
        self.stdin_tx
            .send(line)
            .map_err(|_| anyhow::anyhow!("ACP process stdin closed"))?;

        let expected_id = id;

        // Read events until session completes or we get the prompt response.
        let mut event_rx = self.event_rx.lock().await;
        let mut response_text = String::new();
        let mut assistant_turns = 0u32;

        loop {
            match event_rx.recv().await {
                Some(AcpEvent::TextBlock(text)) => {
                    assistant_turns += 1;

                    // Check turn limit.
                    if let Some(max) = limits.max_turns
                        && assistant_turns > max
                    {
                        warn!(
                            agent = %self.name,
                            turns = assistant_turns,
                            max = max,
                            "turn limit exceeded, killing ACP process"
                        );
                        self.kill().await;
                        bail!(
                            "task killed: exceeded {} turn limit ({} turns)",
                            max,
                            assistant_turns
                        );
                    }

                    if let Some(tx) = progress_tx {
                        let _ = tx.send(text.clone());
                    }
                    response_text.push_str(&text);
                }
                Some(AcpEvent::Response(resp)) => {
                    if resp.id == Some(expected_id) {
                        if let Some(err) = resp.error {
                            bail!("ACP session/prompt failed: {}", err);
                        }
                        // Prompt response received — task is done.
                        break;
                    }
                }
                Some(AcpEvent::PermissionRequest(req_id)) => {
                    let approval = build_permission_approval(req_id);
                    let _ = self.stdin_tx.send(approval);
                }
                Some(AcpEvent::SessionComplete) => {
                    // Session complete notification — task is done.
                    break;
                }
                Some(AcpEvent::ProcessExited) | None => {
                    bail!("ACP process exited mid-task");
                }
            }
        }

        // Update agent state.
        if let Ok(mut state) = agent::load_state(&self.name) {
            if state.config.session == SessionMode::Persistent {
                state.session_id = session_id;
            }
            // ACP doesn't report cost — we track turns only.
            state.total_turns += assistant_turns;
            let _ = agent::save_state_pub(&state);

            // Check budget (cost-based; ACP doesn't report cost, so this is
            // only effective if cost was accumulated from other sources).
            if let Some(budget) = limits.budget_usd
                && state.total_cost >= budget
            {
                warn!(
                    agent = %self.name,
                    cost = state.total_cost,
                    budget = budget,
                    "budget exceeded, killing ACP process"
                );
                self.kill().await;
            }
        }

        Ok(TurnResult {
            response_text,
            session_id: self.session_id.lock().await.clone().unwrap_or_default(),
            cost_usd: 0.0, // ACP doesn't report cost.
            num_turns: assistant_turns,
        })
    }

    /// Kill the running process immediately.
    pub async fn kill(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        warn!(agent = %self.name, "ACP process killed");
    }

    /// Gracefully stop the ACP process.
    pub async fn stop(&self) {
        // Try to cancel the current session if we have one.
        if let Some(sid) = self.session_id.lock().await.clone() {
            let id = *self.next_id.lock().await;
            let req = build_session_cancel(id, &sid);
            if let Ok(line) = req.to_line() {
                let _ = self.stdin_tx.send(line);
            }
        }

        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        info!(agent = %self.name, "ACP process stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonrpc_request_serialization() {
        let req = JsonRpcRequest::new(1, "initialize", Some(serde_json::json!({"key": "val"})));
        let line = req.to_line().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["params"]["key"], "val");
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn test_jsonrpc_request_no_params() {
        let req = JsonRpcRequest::new(42, "test/method", None);
        let line = req.to_line().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "test/method");
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn test_parse_response_success() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"abc-123"}}"#;
        let resp = parse_response(json).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
        assert!(resp.method.is_none());
    }

    #[test]
    fn test_parse_response_error() {
        let json =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let resp = parse_response(json).unwrap();
        assert_eq!(resp.id, Some(2));
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid request");
    }

    #[test]
    fn test_parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"session/update","params":{"status":"working","messages":[]}}"#;
        let resp = parse_response(json).unwrap();
        assert!(is_notification(&resp));
        assert_eq!(resp.method.as_deref(), Some("session/update"));
        assert!(resp.id.is_none());
    }

    #[test]
    fn test_is_notification() {
        let notification = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: None,
            error: None,
            method: Some("session/update".to_string()),
            params: Some(serde_json::json!({})),
        };
        assert!(is_notification(&notification));

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            result: Some(serde_json::json!({})),
            error: None,
            method: None,
            params: None,
        };
        assert!(!is_notification(&response));
    }

    #[test]
    fn test_classify_message_server_request() {
        // id + method = server request (e.g. session/request_permission)
        let server_req = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(42),
            result: None,
            error: None,
            method: Some("session/request_permission".to_string()),
            params: Some(serde_json::json!({"tool": "bash"})),
        };
        assert_eq!(classify_message(&server_req), MessageKind::ServerRequest);
    }

    #[test]
    fn test_classify_message_notification() {
        // method only (no id) = notification
        let notification = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: None,
            error: None,
            method: Some("session/update".to_string()),
            params: Some(serde_json::json!({})),
        };
        assert_eq!(classify_message(&notification), MessageKind::Notification);
    }

    #[test]
    fn test_classify_message_response() {
        // id only (no method) = response to our request
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            result: Some(serde_json::json!({})),
            error: None,
            method: None,
            params: None,
        };
        assert_eq!(classify_message(&response), MessageKind::Response);
    }

    #[test]
    fn test_extract_update_text_with_string_content() {
        let params = serde_json::json!({
            "messages": [{
                "role": "assistant",
                "content": "Hello, I am helping you."
            }]
        });
        let text = extract_update_text(&params).unwrap();
        assert_eq!(text, "Hello, I am helping you.");
    }

    #[test]
    fn test_extract_update_text_with_content_blocks() {
        let params = serde_json::json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Part one. "},
                    {"type": "text", "text": "Part two."}
                ]
            }]
        });
        let text = extract_update_text(&params).unwrap();
        assert_eq!(text, "Part one. Part two.");
    }

    #[test]
    fn test_extract_update_text_skips_user_messages() {
        let params = serde_json::json!({
            "messages": [
                {"role": "user", "content": "User message"},
                {"role": "assistant", "content": "Assistant reply"}
            ]
        });
        let text = extract_update_text(&params).unwrap();
        assert_eq!(text, "Assistant reply");
    }

    #[test]
    fn test_extract_update_text_no_assistant() {
        let params = serde_json::json!({
            "messages": [{"role": "user", "content": "Hello"}]
        });
        assert!(extract_update_text(&params).is_none());
    }

    #[test]
    fn test_extract_update_text_empty_messages() {
        let params = serde_json::json!({"messages": []});
        assert!(extract_update_text(&params).is_none());
    }

    #[test]
    fn test_extract_update_text_no_messages() {
        let params = serde_json::json!({"status": "working"});
        assert!(extract_update_text(&params).is_none());
    }

    #[test]
    fn test_is_session_complete() {
        assert!(is_session_complete(
            &serde_json::json!({"status": "completed"})
        ));
        assert!(is_session_complete(&serde_json::json!({"status": "done"})));
        assert!(is_session_complete(&serde_json::json!({"status": "ended"})));
        assert!(!is_session_complete(
            &serde_json::json!({"status": "working"})
        ));
        assert!(!is_session_complete(&serde_json::json!({})));
    }

    #[test]
    fn test_extract_session_id() {
        let result = serde_json::json!({"sessionId": "sess-abc-123"});
        assert_eq!(
            extract_session_id(&result),
            Some("sess-abc-123".to_string())
        );

        let empty = serde_json::json!({});
        assert!(extract_session_id(&empty).is_none());
    }

    #[test]
    fn test_build_initialize() {
        let req = build_initialize(1);
        assert_eq!(req.method, "initialize");
        assert_eq!(req.id, 1);
        let params = req.params.unwrap();
        assert_eq!(params["capabilities"]["terminal"], false);
        assert_eq!(params["capabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientInfo"]["name"], "deskd");
    }

    #[test]
    fn test_build_session_new() {
        let req = build_session_new(2, "/home/dev", None);
        assert_eq!(req.method, "session/new");
        let params = req.params.unwrap();
        assert_eq!(params["cwd"], "/home/dev");
    }

    #[test]
    fn test_build_session_new_with_mcp() {
        let mut servers = HashMap::new();
        servers.insert(
            "deskd".to_string(),
            serde_json::json!({"command": "deskd", "args": ["mcp"]}),
        );
        let req = build_session_new(3, "/home/dev", Some(servers));
        let params = req.params.unwrap();
        assert_eq!(params["mcpServers"]["deskd"]["command"], "deskd");
    }

    #[test]
    fn test_build_session_load() {
        let req = build_session_load(4, "sess-123");
        assert_eq!(req.method, "session/load");
        let params = req.params.unwrap();
        assert_eq!(params["sessionId"], "sess-123");
    }

    #[test]
    fn test_build_session_prompt() {
        let req = build_session_prompt(5, "sess-123", "Write tests");
        assert_eq!(req.method, "session/prompt");
        let params = req.params.unwrap();
        assert_eq!(params["sessionId"], "sess-123");
        assert_eq!(params["text"], "Write tests");
    }

    #[test]
    fn test_build_session_cancel() {
        let req = build_session_cancel(6, "sess-123");
        assert_eq!(req.method, "session/cancel");
        let params = req.params.unwrap();
        assert_eq!(params["sessionId"], "sess-123");
    }

    #[test]
    fn test_build_permission_approval() {
        let line = build_permission_approval(42);
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["result"]["approved"], true);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn test_jsonrpc_error_display() {
        let err = JsonRpcError {
            code: -32600,
            message: "Invalid request".to_string(),
            data: None,
        };
        assert_eq!(format!("{}", err), "JSON-RPC error -32600: Invalid request");
    }
}
