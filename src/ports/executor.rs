//! Executor port — abstraction over LLM execution backends.
//!
//! Claude CLI, ACP, Gemini, Ollama — all implement this trait.
//! The executor is stateless infrastructure: it receives a task message
//! and returns a result. It does not own context or state.

use anyhow::Result;

/// Accumulated token usage across all messages in a task.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    /// Merge another `TokenUsage` into this one (struct-to-struct).
    pub fn merge(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    /// Accumulate usage from a parsed JSON value.
    /// Expects the `usage` object from a Claude assistant message.
    pub fn accumulate(&mut self, usage: &serde_json::Value) {
        if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            self.input_tokens += v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
            self.output_tokens += v;
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.cache_creation_input_tokens += v;
        }
        if let Some(v) = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.cache_read_input_tokens += v;
        }
    }
}

/// Resource limits for a single task execution.
pub struct TaskLimits {
    /// Max assistant turns (tool-use loops) before killing the process.
    pub max_turns: Option<u32>,
    /// Max cumulative cost (USD) for this agent before killing.
    pub budget_usd: Option<f64>,
}

/// Result of a single executor turn (task completion).
pub struct TurnResult {
    pub response_text: String,
    pub session_id: String,
    pub cost_usd: f64,
    pub num_turns: u32,
    pub token_usage: TokenUsage,
}

/// Abstraction over LLM execution backends.
///
/// Implementations manage a long-lived subprocess (Claude CLI, ACP server, etc.)
/// and accept tasks via `send_task`. The executor handles streaming, progress
/// reporting, and session management internally.
pub trait Executor: Send {
    /// Send a task to the executor and wait for completion.
    ///
    /// `progress_tx` receives streaming text chunks for real-time progress.
    fn send_task(
        &self,
        message: &str,
        progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
        limits: &TaskLimits,
    ) -> impl std::future::Future<Output = Result<TurnResult>> + Send;

    /// Check if the underlying process is alive.
    fn is_alive(&self) -> impl std::future::Future<Output = bool> + Send;

    /// Gracefully stop the executor.
    fn stop(&self) -> impl std::future::Future<Output = ()> + Send;

    /// Forcefully kill the executor.
    fn kill(&self) -> impl std::future::Future<Output = ()> + Send;
}
