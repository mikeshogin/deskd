//! Executable skill graph engine.
//!
//! A skill graph is a DAG (directed acyclic graph) of **tool call groups** with
//! **LLM decision nodes** between them.  Tool groups execute directly — no LLM
//! interpretation.  LLM nodes receive results from upstream groups and make
//! routing/planning decisions.
//!
//! ## Key concepts
//!
//! - **Step** — a named node in the DAG.  Either a tool group or an LLM node.
//! - **Tool group** — one or more tool calls executed directly.  Can run in parallel.
//! - **LLM decision node** — sends upstream results + a prompt to Claude, gets a decision.
//! - **Dependencies** — `depends_on` edges that define execution order.
//! - **Conditions** — steps can be skipped based on results from upstream steps.
//! - **Templates** — tool args can reference `{variable}` from upstream LLM outputs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// YAML schema
// ---------------------------------------------------------------------------

/// Top-level graph definition loaded from YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDef {
    /// Human-readable graph name.
    pub graph: String,
    /// Schema version (currently 1).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Optional description.
    #[serde(default)]
    pub description: String,
    /// Ordered list of steps (order is informational — execution order comes from DAG).
    pub steps: Vec<StepDef>,
    /// Optional templates directory (relative to graph file).
    #[serde(default)]
    pub templates_dir: Option<String>,
}

fn default_version() -> u32 {
    1
}

/// A single step in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDef {
    /// Unique step identifier.
    pub id: String,
    /// Step type: "tool" (default) or "llm".
    #[serde(default = "default_step_type", rename = "type")]
    pub step_type: StepType,
    /// IDs of steps that must complete before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Whether tools in this group run in parallel.
    #[serde(default)]
    pub parallel: bool,
    /// Condition expression — step is skipped if this evaluates to false.
    /// References variables from upstream LLM outputs.
    #[serde(default)]
    pub condition: Option<String>,
    /// Tool calls (for tool-type steps).
    #[serde(default)]
    pub tools: Vec<ToolCall>,
    /// LLM prompt (for llm-type steps).
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StepType {
    Tool,
    Llm,
}

fn default_step_type() -> StepType {
    StepType::Tool
}

/// A single tool invocation within a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name — "bash", "write_file", "read_file", etc.
    pub tool: String,
    /// Arguments (command string for bash, path for file ops).
    #[serde(default)]
    pub args: String,
    /// For write_file: template name to render.
    #[serde(default)]
    pub template: Option<String>,
    /// For write_file: target path.
    #[serde(default)]
    pub path: Option<String>,
    /// Per-tool condition.
    #[serde(default)]
    pub condition: Option<String>,
}

// ---------------------------------------------------------------------------
// Execution context — accumulates results as the graph runs
// ---------------------------------------------------------------------------

/// Result of executing a single tool call.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub tool: String,
    pub args: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub skipped: bool,
}

/// Result of a single step execution.
#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub id: String,
    pub step_type: String,
    pub skipped: bool,
    /// Tool results (for tool steps).
    pub tool_results: Vec<ToolResult>,
    /// LLM output text (for llm steps).
    pub llm_output: Option<String>,
    pub duration_ms: u64,
}

/// Accumulated execution state — passed to LLM nodes and used for conditions.
#[derive(Debug, Clone, Default)]
pub struct ExecContext {
    /// Step ID → StepResult.
    pub results: HashMap<String, StepResult>,
    /// Variables extracted from LLM outputs (JSON keys flattened).
    pub variables: HashMap<String, String>,
}

impl ExecContext {
    /// Build LLM context from the full transitive subgraph using compression.
    ///
    /// LLM nodes act as compression boundaries: they already "digested" everything
    /// upstream of them.  So the context for a new LLM node is:
    ///
    /// 1. Find the nearest LLM ancestor(s) in the transitive subgraph
    /// 2. Include their output (compressed knowledge)
    /// 3. Include full tool results for steps *between* those LLM nodes and current
    /// 4. Skip anything upstream of a compression boundary
    ///
    /// This keeps context bounded even for deep graphs.
    pub fn llm_context(
        &self,
        current_step: &str,
        graph: &GraphDef,
        exec_order: &[String],
    ) -> String {
        let step_map: HashMap<&str, &StepDef> =
            graph.steps.iter().map(|s| (s.id.as_str(), s)).collect();

        // Compute transitive dependencies (all ancestors).
        let ancestors = transitive_deps(current_step, &step_map);

        // Walk execution order (topological) and find the last LLM boundary.
        // Everything after that boundary (in exec order) gets full context.
        // The boundary itself gets included as compressed output only.
        let mut last_llm_boundary: Option<&str> = None;
        for id in exec_order {
            if !ancestors.contains(id.as_str()) {
                continue;
            }
            if let Some(result) = self.results.get(id.as_str())
                && result.step_type == "llm"
                && !result.skipped
            {
                last_llm_boundary = Some(id.as_str());
            }
        }

        let mut parts = Vec::new();

        // If there's a compression boundary, include its output first.
        if let Some(boundary_id) = last_llm_boundary
            && let Some(result) = self.results.get(boundary_id)
            && let Some(ref output) = result.llm_output
        {
            parts.push(format!(
                "## Summary from step `{}` (compressed)\n{}\n",
                boundary_id, output
            ));
        }

        // Include full results for ancestors that come after the boundary.
        let after_boundary = last_llm_boundary.is_none(); // if no boundary, include everything
        let mut past_boundary = after_boundary;
        for id in exec_order {
            if !ancestors.contains(id.as_str()) {
                continue;
            }
            if let Some(bid) = last_llm_boundary
                && id == bid
            {
                past_boundary = true;
                continue; // skip the boundary itself (already included above)
            }
            if !past_boundary {
                continue;
            }
            if let Some(result) = self.results.get(id.as_str()) {
                parts.push(format_step_result(id, result));
            }
        }

        parts.join("\n")
    }

    /// Collect all results from a set of dependency step IDs into a summary string
    /// suitable for LLM prompts. (Legacy — used when graph/order not available.)
    pub fn upstream_summary(&self, dep_ids: &[String]) -> String {
        let mut parts = Vec::new();
        for dep_id in dep_ids {
            if let Some(result) = self.results.get(dep_id.as_str()) {
                parts.push(format_step_result(dep_id, result));
            }
        }
        parts.join("\n")
    }

    /// Evaluate a simple condition expression against accumulated variables.
    ///
    /// Supported forms:
    /// - `"'value' in variable_name"` — true if variable contains the value
    /// - `"'value' not in variable_name"` — negation
    /// - `"variable_name"` — true if variable is truthy (non-empty, not "false")
    /// - `"!variable_name"` — negation
    pub fn eval_condition(&self, condition: &str) -> bool {
        let cond = condition.trim();

        // Handle "'value' in var" and "'value' not in var"
        if let Some(rest) = cond.strip_prefix('\'')
            && let Some(quote_end) = rest.find('\'')
        {
            let value = &rest[..quote_end];
            let remainder = rest[quote_end + 1..].trim();

            if let Some(var_name) = remainder.strip_prefix("not in ") {
                let var_name = var_name.trim();
                return self
                    .variables
                    .get(var_name)
                    .map(|v| !v.contains(value))
                    .unwrap_or(true);
            }
            if let Some(var_name) = remainder.strip_prefix("in ") {
                let var_name = var_name.trim();
                return self
                    .variables
                    .get(var_name)
                    .map(|v| v.contains(value))
                    .unwrap_or(false);
            }
        }

        // Handle "!var" — negation
        if let Some(var_name) = cond.strip_prefix('!') {
            let var_name = var_name.trim();
            return self
                .variables
                .get(var_name)
                .map(|v| v.is_empty() || v == "false" || v == "0")
                .unwrap_or(true);
        }

        // Handle "var" — truthy check
        self.variables
            .get(cond)
            .map(|v| !v.is_empty() && v != "false" && v != "0")
            .unwrap_or(false)
    }

    /// Expand `{variable}` placeholders in a string using accumulated variables.
    pub fn expand_template(&self, input: &str) -> String {
        let mut result = input.to_string();
        for (key, value) in &self.variables {
            result = result.replace(&format!("{{{}}}", key), value);
        }
        result
    }

    /// Extract JSON keys from LLM output and add them as variables.
    pub fn extract_variables_from_json(&mut self, step_id: &str, text: &str) {
        // Try to find JSON in the output (may be wrapped in markdown code blocks).
        let json_str = extract_json_block(text);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) {
            self.flatten_json(step_id, &value, String::new());
        }
    }

    fn flatten_json(&mut self, step_id: &str, value: &serde_json::Value, prefix: String) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{}.{}", prefix, k)
                    };
                    self.flatten_json(step_id, v, key);
                }
            }
            serde_json::Value::Array(arr) => {
                // Store array as JSON string for "in" checks.
                let json_str = serde_json::to_string(value).unwrap_or_default();
                let key = if prefix.is_empty() {
                    step_id.to_string()
                } else {
                    prefix.clone()
                };
                self.variables.insert(key, json_str);
                for (i, v) in arr.iter().enumerate() {
                    self.flatten_json(step_id, v, format!("{}.{}", prefix, i));
                }
            }
            serde_json::Value::String(s) => {
                let key = if prefix.is_empty() {
                    step_id.to_string()
                } else {
                    prefix
                };
                self.variables.insert(key, s.clone());
            }
            serde_json::Value::Bool(b) => {
                let key = if prefix.is_empty() {
                    step_id.to_string()
                } else {
                    prefix
                };
                self.variables.insert(key, b.to_string());
            }
            serde_json::Value::Number(n) => {
                let key = if prefix.is_empty() {
                    step_id.to_string()
                } else {
                    prefix
                };
                self.variables.insert(key, n.to_string());
            }
            serde_json::Value::Null => {}
        }
    }
}

/// Format a single step result for inclusion in LLM context.
fn format_step_result(step_id: &str, result: &StepResult) -> String {
    if result.skipped {
        return format!("## Step `{}` — SKIPPED\n", step_id);
    }
    match result.step_type.as_str() {
        "llm" => {
            if let Some(ref output) = result.llm_output {
                format!("## Step `{}`\n{}\n", step_id, output)
            } else {
                String::new()
            }
        }
        _ => {
            let mut step_parts = Vec::new();
            for tr in &result.tool_results {
                if tr.skipped {
                    continue;
                }
                step_parts.push(format!(
                    "### `{} {}`\nExit code: {}\nStdout:\n```\n{}\n```{}",
                    tr.tool,
                    tr.args,
                    tr.exit_code,
                    tr.stdout.trim(),
                    if tr.stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\nStderr:\n```\n{}\n```", tr.stderr.trim())
                    }
                ));
            }
            format!("## Step `{}`\n{}\n", step_id, step_parts.join("\n"))
        }
    }
}

/// Compute transitive dependencies for a step (all ancestors in the DAG).
fn transitive_deps<'a>(step_id: &str, step_map: &HashMap<&'a str, &'a StepDef>) -> HashSet<String> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    if let Some(step) = step_map.get(step_id) {
        for dep in &step.depends_on {
            queue.push_back(dep.clone());
        }
    }
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(step) = step_map.get(id.as_str()) {
            for dep in &step.depends_on {
                queue.push_back(dep.clone());
            }
        }
    }
    visited
}

/// Extract a JSON block from text that may be wrapped in ```json ... ``` markers.
fn extract_json_block(text: &str) -> String {
    // Try to find ```json ... ``` block.
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // Try to find ``` ... ``` block.
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // Try the whole text as JSON.
    let trimmed = text.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return trimmed.to_string();
    }
    trimmed.to_string()
}

// ---------------------------------------------------------------------------
// DAG validation and topological sort
// ---------------------------------------------------------------------------

/// Validate the graph and return execution order (topological sort).
pub fn topo_sort(graph: &GraphDef) -> Result<Vec<String>> {
    let step_ids: HashSet<&str> = graph.steps.iter().map(|s| s.id.as_str()).collect();

    // Validate: all depends_on references exist.
    for step in &graph.steps {
        for dep in &step.depends_on {
            if !step_ids.contains(dep.as_str()) {
                bail!(
                    "step '{}' depends on '{}' which does not exist",
                    step.id,
                    dep
                );
            }
        }
    }

    // Build adjacency list and in-degree map.
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for step in &graph.steps {
        in_degree.entry(step.id.as_str()).or_insert(0);
        for dep in &step.depends_on {
            *in_degree.entry(step.id.as_str()).or_insert(0) += 1;
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(step.id.as_str());
        }
    }

    // Kahn's algorithm.
    let mut queue: VecDeque<&str> = VecDeque::new();
    for step in &graph.steps {
        if in_degree[step.id.as_str()] == 0 {
            queue.push_back(step.id.as_str());
        }
    }

    let mut order = Vec::new();
    while let Some(node) = queue.pop_front() {
        order.push(node.to_string());
        if let Some(deps) = dependents.get(node) {
            for dep in deps {
                let degree = in_degree.get_mut(dep).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(dep);
                }
            }
        }
    }

    if order.len() != graph.steps.len() {
        bail!("graph contains a cycle — not a valid DAG");
    }

    Ok(order)
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Execute a single bash command. Returns stdout, stderr, exit code.
async fn exec_bash(cmd: &str, work_dir: &Path) -> Result<(String, String, i32)> {
    let child = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn bash")?;

    let output = child.wait_with_output().await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok((stdout, stderr, exit_code))
}

/// Execute a write_file tool call — render template and write to path.
async fn exec_write_file(
    path: &str,
    content: &str,
    work_dir: &Path,
) -> Result<(String, String, i32)> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_string());
        format!("{}/{}", home, rest)
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        work_dir.join(path).to_string_lossy().to_string()
    };

    // Create parent directories.
    if let Some(parent) = Path::new(&expanded).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("failed to create parent dirs")?;
    }

    tokio::fs::write(&expanded, content)
        .await
        .context("failed to write file")?;

    Ok((
        format!("wrote {} bytes to {}", content.len(), expanded),
        String::new(),
        0,
    ))
}

/// Execute a read_file tool call.
async fn exec_read_file(path: &str, work_dir: &Path) -> Result<(String, String, i32)> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_string());
        format!("{}/{}", home, rest)
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        work_dir.join(path).to_string_lossy().to_string()
    };

    match tokio::fs::read_to_string(&expanded).await {
        Ok(content) => Ok((content, String::new(), 0)),
        Err(e) => Ok((String::new(), format!("read error: {}", e), 1)),
    }
}

/// Execute a single tool call, expanding templates from context.
async fn exec_tool(
    tool: &ToolCall,
    ctx: &ExecContext,
    work_dir: &Path,
    templates_dir: Option<&Path>,
) -> ToolResult {
    // Check per-tool condition.
    if let Some(ref cond) = tool.condition
        && !ctx.eval_condition(cond)
    {
        return ToolResult {
            tool: tool.tool.clone(),
            args: tool.args.clone(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            skipped: true,
        };
    }

    let expanded_args = ctx.expand_template(&tool.args);

    let result = match tool.tool.as_str() {
        "bash" => exec_bash(&expanded_args, work_dir).await,
        "write_file" => {
            let path = tool.path.as_deref().unwrap_or(&expanded_args);
            let path = ctx.expand_template(path);

            // If template is specified, load and expand it.
            let content = if let Some(ref tmpl_name) = tool.template {
                let tmpl_path = templates_dir
                    .map(|d| d.join(tmpl_name))
                    .unwrap_or_else(|| Path::new(tmpl_name).to_path_buf());
                match tokio::fs::read_to_string(&tmpl_path).await {
                    Ok(tmpl) => ctx.expand_template(&tmpl),
                    Err(e) => {
                        return ToolResult {
                            tool: tool.tool.clone(),
                            args: format!("template: {}", tmpl_name),
                            stdout: String::new(),
                            stderr: format!("failed to read template {}: {}", tmpl_name, e),
                            exit_code: 1,
                            skipped: false,
                        };
                    }
                }
            } else {
                ctx.expand_template(&expanded_args)
            };

            exec_write_file(&path, &content, work_dir).await
        }
        "read_file" => {
            let path = tool.path.as_deref().unwrap_or(&expanded_args);
            let path = ctx.expand_template(path);
            exec_read_file(&path, work_dir).await
        }
        other => Ok((String::new(), format!("unknown tool: {}", other), 127)),
    };

    match result {
        Ok((stdout, stderr, exit_code)) => ToolResult {
            tool: tool.tool.clone(),
            args: expanded_args,
            stdout,
            stderr,
            exit_code,
            skipped: false,
        },
        Err(e) => ToolResult {
            tool: tool.tool.clone(),
            args: expanded_args,
            stdout: String::new(),
            stderr: format!("execution error: {}", e),
            exit_code: -1,
            skipped: false,
        },
    }
}

// ---------------------------------------------------------------------------
// LLM execution (calls Claude via CLI)
// ---------------------------------------------------------------------------

/// Call Claude with a prompt and get a response.
/// Uses `claude` CLI in non-interactive mode.
async fn exec_llm(prompt: &str, work_dir: &Path) -> Result<String> {
    let child = Command::new("claude")
        .args(["--output-format", "text", "-p", prompt])
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn claude CLI for LLM node")?;

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("claude CLI failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout)
}

// ---------------------------------------------------------------------------
// Graph executor
// ---------------------------------------------------------------------------

/// Progress callback type — called after each step completes.
pub type ProgressFn = Box<dyn Fn(&StepResult) + Send + Sync>;

/// Execute a graph definition end-to-end.
///
/// Returns the accumulated execution context with all step results.
pub async fn execute(
    graph: &GraphDef,
    work_dir: &Path,
    on_progress: Option<&ProgressFn>,
) -> Result<ExecContext> {
    let order = topo_sort(graph)?;
    let step_map: HashMap<&str, &StepDef> =
        graph.steps.iter().map(|s| (s.id.as_str(), s)).collect();

    let templates_dir = graph.templates_dir.as_ref().map(|d| work_dir.join(d));
    let templates_path = templates_dir.as_deref();

    let mut ctx = ExecContext::default();

    for step_id in &order {
        let step = step_map[step_id.as_str()];
        let start = std::time::Instant::now();

        info!(step = %step_id, step_type = ?step.step_type, "executing graph step");

        // Check step-level condition.
        if let Some(ref cond) = step.condition
            && !ctx.eval_condition(cond)
        {
            info!(step = %step_id, condition = %cond, "step skipped (condition false)");
            let result = StepResult {
                id: step_id.clone(),
                step_type: format!("{:?}", step.step_type).to_lowercase(),
                skipped: true,
                tool_results: Vec::new(),
                llm_output: None,
                duration_ms: start.elapsed().as_millis() as u64,
            };
            if let Some(cb) = on_progress {
                cb(&result);
            }
            ctx.results.insert(step_id.clone(), result);
            continue;
        }

        match step.step_type {
            StepType::Tool => {
                let tool_results = if step.parallel && step.tools.len() > 1 {
                    // Execute tools in parallel.
                    let futures: Vec<_> = step
                        .tools
                        .iter()
                        .map(|t| exec_tool(t, &ctx, work_dir, templates_path))
                        .collect();
                    futures::future::join_all(futures).await
                } else {
                    // Execute tools sequentially.
                    let mut results = Vec::new();
                    for tool in &step.tools {
                        let tr = exec_tool(tool, &ctx, work_dir, templates_path).await;
                        // Log non-zero exit codes.
                        if tr.exit_code != 0 && !tr.skipped {
                            warn!(
                                step = %step_id,
                                tool = %tr.tool,
                                exit_code = tr.exit_code,
                                stderr = %tr.stderr,
                                "tool returned non-zero exit code"
                            );
                        }
                        results.push(tr);
                    }
                    results
                };

                let result = StepResult {
                    id: step_id.clone(),
                    step_type: "tool".to_string(),
                    skipped: false,
                    tool_results,
                    llm_output: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                };

                if let Some(cb) = on_progress {
                    cb(&result);
                }
                ctx.results.insert(step_id.clone(), result);
            }

            StepType::Llm => {
                let prompt_template = step
                    .prompt
                    .as_deref()
                    .unwrap_or("Analyze the results from previous steps.");

                // Build prompt with full transitive context using compression.
                // LLM ancestors act as compression boundaries.
                let upstream = ctx.llm_context(step_id, graph, &order);
                let expanded_prompt = ctx.expand_template(prompt_template);
                let full_prompt = format!(
                    "# Context from previous steps\n\n{}\n\n# Your task\n\n{}",
                    upstream, expanded_prompt
                );

                debug!(step = %step_id, prompt_len = full_prompt.len(), "calling LLM");

                match exec_llm(&full_prompt, work_dir).await {
                    Ok(output) => {
                        info!(step = %step_id, output_len = output.len(), "LLM node completed");

                        // Extract variables from JSON in LLM output.
                        ctx.extract_variables_from_json(step_id, &output);

                        let result = StepResult {
                            id: step_id.clone(),
                            step_type: "llm".to_string(),
                            skipped: false,
                            tool_results: Vec::new(),
                            llm_output: Some(output),
                            duration_ms: start.elapsed().as_millis() as u64,
                        };

                        if let Some(cb) = on_progress {
                            cb(&result);
                        }
                        ctx.results.insert(step_id.clone(), result);
                    }
                    Err(e) => {
                        warn!(step = %step_id, error = %e, "LLM node failed");
                        let result = StepResult {
                            id: step_id.clone(),
                            step_type: "llm".to_string(),
                            skipped: false,
                            tool_results: Vec::new(),
                            llm_output: Some(format!("ERROR: {}", e)),
                            duration_ms: start.elapsed().as_millis() as u64,
                        };
                        if let Some(cb) = on_progress {
                            cb(&result);
                        }
                        ctx.results.insert(step_id.clone(), result);
                    }
                }
            }
        }
    }

    Ok(ctx)
}

// ---------------------------------------------------------------------------
// Load from file
// ---------------------------------------------------------------------------

/// Load a graph definition from a YAML file.
pub fn load(path: &Path) -> Result<GraphDef> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading graph: {}", path.display()))?;
    let graph: GraphDef = serde_yaml::from_str(&content)
        .with_context(|| format!("parsing graph: {}", path.display()))?;

    // Validate.
    topo_sort(&graph)?;

    Ok(graph)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_graph() -> GraphDef {
        serde_yaml::from_str(
            r#"
graph: test-graph
version: 1
steps:
  - id: step_a
    tools:
      - { tool: "bash", args: "echo hello" }
  - id: step_b
    depends_on: [step_a]
    parallel: true
    tools:
      - { tool: "bash", args: "echo world" }
      - { tool: "bash", args: "echo foo" }
  - id: step_c
    type: llm
    depends_on: [step_b]
    prompt: "Summarize the results."
"#,
        )
        .unwrap()
    }

    #[test]
    fn test_topo_sort_valid() {
        let graph = sample_graph();
        let order = topo_sort(&graph).unwrap();
        assert_eq!(order, vec!["step_a", "step_b", "step_c"]);
    }

    #[test]
    fn test_topo_sort_cycle() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: cycle
steps:
  - id: a
    depends_on: [b]
    tools: [{ tool: "bash", args: "echo a" }]
  - id: b
    depends_on: [a]
    tools: [{ tool: "bash", args: "echo b" }]
"#,
        )
        .unwrap();
        assert!(topo_sort(&graph).is_err());
    }

    #[test]
    fn test_topo_sort_missing_dep() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: bad-dep
steps:
  - id: a
    depends_on: [nonexistent]
    tools: [{ tool: "bash", args: "echo a" }]
"#,
        )
        .unwrap();
        assert!(topo_sort(&graph).is_err());
    }

    #[test]
    fn test_topo_sort_parallel_roots() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: parallel-roots
steps:
  - id: a
    tools: [{ tool: "bash", args: "echo a" }]
  - id: b
    tools: [{ tool: "bash", args: "echo b" }]
  - id: c
    depends_on: [a, b]
    tools: [{ tool: "bash", args: "echo c" }]
"#,
        )
        .unwrap();
        let order = topo_sort(&graph).unwrap();
        // a and b can be in either order, but c must be last.
        assert_eq!(order.last().unwrap(), "c");
        assert!(order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
    }

    #[test]
    fn test_condition_in() {
        let mut ctx = ExecContext::default();
        ctx.variables
            .insert("missing_tools".to_string(), r#"["cargo","gh"]"#.to_string());

        assert!(ctx.eval_condition("'cargo' in missing_tools"));
        assert!(ctx.eval_condition("'gh' in missing_tools"));
        assert!(!ctx.eval_condition("'claude' in missing_tools"));
        assert!(ctx.eval_condition("'claude' not in missing_tools"));
    }

    #[test]
    fn test_condition_truthy() {
        let mut ctx = ExecContext::default();
        ctx.variables
            .insert("needs_gh_auth".to_string(), "true".to_string());
        ctx.variables.insert("empty_var".to_string(), String::new());

        assert!(ctx.eval_condition("needs_gh_auth"));
        assert!(!ctx.eval_condition("!needs_gh_auth"));
        assert!(!ctx.eval_condition("empty_var"));
        assert!(ctx.eval_condition("!empty_var"));
        assert!(!ctx.eval_condition("nonexistent"));
        assert!(ctx.eval_condition("!nonexistent"));
    }

    #[test]
    fn test_expand_template() {
        let mut ctx = ExecContext::default();
        ctx.variables
            .insert("agent_name".to_string(), "kira".to_string());
        ctx.variables
            .insert("model".to_string(), "claude-opus-4-6".to_string());

        assert_eq!(
            ctx.expand_template("Hello {agent_name}, using {model}"),
            "Hello kira, using claude-opus-4-6"
        );
    }

    #[test]
    fn test_extract_json_variables() {
        let mut ctx = ExecContext::default();
        let output = r#"Here's my analysis:
```json
{"missing_tools": ["cargo", "gh"], "pkg_manager": "apt", "needs_gh_auth": true}
```
"#;
        ctx.extract_variables_from_json("plan", output);

        assert_eq!(ctx.variables["pkg_manager"], "apt");
        assert_eq!(ctx.variables["needs_gh_auth"], "true");
        assert!(ctx.variables["missing_tools"].contains("cargo"));
    }

    #[test]
    fn test_upstream_summary() {
        let mut ctx = ExecContext::default();
        ctx.results.insert(
            "step_a".to_string(),
            StepResult {
                id: "step_a".to_string(),
                step_type: "tool".to_string(),
                skipped: false,
                tool_results: vec![ToolResult {
                    tool: "bash".to_string(),
                    args: "echo hello".to_string(),
                    stdout: "hello\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    skipped: false,
                }],
                llm_output: None,
                duration_ms: 100,
            },
        );

        let summary = ctx.upstream_summary(&["step_a".to_string()]);
        assert!(summary.contains("hello"));
        assert!(summary.contains("step_a"));
    }

    #[test]
    fn test_llm_context_compression_boundary() {
        // Graph: tool_a → llm_b → tool_c → llm_d (current)
        // llm_d should see: llm_b output (compressed) + tool_c full output
        // It should NOT see tool_a full output (behind the boundary).
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: compression-test
steps:
  - id: tool_a
    tools: [{ tool: "bash", args: "echo raw_data" }]
  - id: llm_b
    type: llm
    depends_on: [tool_a]
    prompt: "Summarize"
  - id: tool_c
    depends_on: [llm_b]
    tools: [{ tool: "bash", args: "echo new_data" }]
  - id: llm_d
    type: llm
    depends_on: [tool_c]
    prompt: "Decide"
"#,
        )
        .unwrap();

        let order = topo_sort(&graph).unwrap();

        let mut ctx = ExecContext::default();
        ctx.results.insert(
            "tool_a".to_string(),
            StepResult {
                id: "tool_a".to_string(),
                step_type: "tool".to_string(),
                skipped: false,
                tool_results: vec![ToolResult {
                    tool: "bash".to_string(),
                    args: "echo raw_data".to_string(),
                    stdout: "raw_data\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    skipped: false,
                }],
                llm_output: None,
                duration_ms: 10,
            },
        );
        ctx.results.insert(
            "llm_b".to_string(),
            StepResult {
                id: "llm_b".to_string(),
                step_type: "llm".to_string(),
                skipped: false,
                tool_results: Vec::new(),
                llm_output: Some("The data says raw_data — all good.".to_string()),
                duration_ms: 500,
            },
        );
        ctx.results.insert(
            "tool_c".to_string(),
            StepResult {
                id: "tool_c".to_string(),
                step_type: "tool".to_string(),
                skipped: false,
                tool_results: vec![ToolResult {
                    tool: "bash".to_string(),
                    args: "echo new_data".to_string(),
                    stdout: "new_data\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    skipped: false,
                }],
                llm_output: None,
                duration_ms: 10,
            },
        );

        let context = ctx.llm_context("llm_d", &graph, &order);

        // Should contain llm_b output (compression boundary)
        assert!(context.contains("compressed"));
        assert!(context.contains("The data says raw_data"));

        // Should contain tool_c output (after boundary)
        assert!(context.contains("new_data"));

        // Should NOT contain tool_a raw output (before boundary)
        assert!(
            !context.contains("### `bash echo raw_data`"),
            "tool_a raw output should be hidden behind compression boundary"
        );
    }

    #[test]
    fn test_llm_context_no_boundary() {
        // Graph: tool_a → tool_b → llm_c (current)
        // No LLM boundary — llm_c should see both tool_a and tool_b.
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: no-boundary
steps:
  - id: tool_a
    tools: [{ tool: "bash", args: "echo first" }]
  - id: tool_b
    depends_on: [tool_a]
    tools: [{ tool: "bash", args: "echo second" }]
  - id: llm_c
    type: llm
    depends_on: [tool_b]
    prompt: "Analyze"
"#,
        )
        .unwrap();

        let order = topo_sort(&graph).unwrap();

        let mut ctx = ExecContext::default();
        ctx.results.insert(
            "tool_a".to_string(),
            StepResult {
                id: "tool_a".to_string(),
                step_type: "tool".to_string(),
                skipped: false,
                tool_results: vec![ToolResult {
                    tool: "bash".to_string(),
                    args: "echo first".to_string(),
                    stdout: "first\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    skipped: false,
                }],
                llm_output: None,
                duration_ms: 10,
            },
        );
        ctx.results.insert(
            "tool_b".to_string(),
            StepResult {
                id: "tool_b".to_string(),
                step_type: "tool".to_string(),
                skipped: false,
                tool_results: vec![ToolResult {
                    tool: "bash".to_string(),
                    args: "echo second".to_string(),
                    stdout: "second\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    skipped: false,
                }],
                llm_output: None,
                duration_ms: 10,
            },
        );

        let context = ctx.llm_context("llm_c", &graph, &order);

        // Without a boundary, both tools should be fully visible.
        assert!(context.contains("first"));
        assert!(context.contains("second"));
    }

    #[test]
    fn test_transitive_deps() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: deps
steps:
  - id: a
    tools: [{ tool: "bash", args: "echo a" }]
  - id: b
    depends_on: [a]
    tools: [{ tool: "bash", args: "echo b" }]
  - id: c
    depends_on: [b]
    tools: [{ tool: "bash", args: "echo c" }]
  - id: d
    depends_on: [c, a]
    tools: [{ tool: "bash", args: "echo d" }]
"#,
        )
        .unwrap();

        let step_map: HashMap<&str, &StepDef> =
            graph.steps.iter().map(|s| (s.id.as_str(), s)).collect();

        let deps = transitive_deps("d", &step_map);
        assert!(deps.contains("a"));
        assert!(deps.contains("b"));
        assert!(deps.contains("c"));
        assert!(!deps.contains("d")); // self not included
        assert_eq!(deps.len(), 3);
    }

    #[tokio::test]
    async fn test_exec_bash() {
        let dir = std::env::temp_dir();
        let (stdout, stderr, code) = exec_bash("echo hello world", &dir).await.unwrap();
        assert_eq!(stdout.trim(), "hello world");
        assert!(stderr.is_empty());
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn test_exec_bash_failure() {
        let dir = std::env::temp_dir();
        let (_, _, code) = exec_bash("exit 42", &dir).await.unwrap();
        assert_eq!(code, 42);
    }

    #[tokio::test]
    async fn test_exec_tool_with_condition_skip() {
        let ctx = ExecContext::default();
        let tool = ToolCall {
            tool: "bash".to_string(),
            args: "echo should_not_run".to_string(),
            template: None,
            path: None,
            condition: Some("nonexistent_var".to_string()),
        };
        let result = exec_tool(&tool, &ctx, &std::env::temp_dir(), None).await;
        assert!(result.skipped);
    }

    #[tokio::test]
    async fn test_execute_simple_graph() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: simple
steps:
  - id: greet
    tools:
      - { tool: "bash", args: "echo hello" }
  - id: shout
    depends_on: [greet]
    tools:
      - { tool: "bash", args: "echo WORLD" }
"#,
        )
        .unwrap();

        let dir = std::env::temp_dir();
        let ctx = execute(&graph, &dir, None).await.unwrap();

        assert_eq!(ctx.results.len(), 2);
        assert!(!ctx.results["greet"].skipped);
        assert!(!ctx.results["shout"].skipped);
        assert_eq!(ctx.results["greet"].tool_results[0].stdout.trim(), "hello");
        assert_eq!(ctx.results["shout"].tool_results[0].stdout.trim(), "WORLD");
    }

    #[tokio::test]
    async fn test_execute_parallel_tools() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: parallel
steps:
  - id: multi
    parallel: true
    tools:
      - { tool: "bash", args: "echo one" }
      - { tool: "bash", args: "echo two" }
      - { tool: "bash", args: "echo three" }
"#,
        )
        .unwrap();

        let dir = std::env::temp_dir();
        let ctx = execute(&graph, &dir, None).await.unwrap();

        let results = &ctx.results["multi"].tool_results;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].stdout.trim(), "one");
        assert_eq!(results[1].stdout.trim(), "two");
        assert_eq!(results[2].stdout.trim(), "three");
    }

    #[tokio::test]
    async fn test_execute_conditional_skip() {
        let graph: GraphDef = serde_yaml::from_str(
            r#"
graph: conditional
steps:
  - id: always
    tools:
      - { tool: "bash", args: "echo ran" }
  - id: skipped
    depends_on: [always]
    condition: "nonexistent_var"
    tools:
      - { tool: "bash", args: "echo should_not_run" }
"#,
        )
        .unwrap();

        let dir = std::env::temp_dir();
        let ctx = execute(&graph, &dir, None).await.unwrap();

        assert!(!ctx.results["always"].skipped);
        assert!(ctx.results["skipped"].skipped);
    }

    #[tokio::test]
    async fn test_execute_write_and_read_file() {
        let dir = std::env::temp_dir().join("graph_test_wr");
        let _ = std::fs::create_dir_all(&dir);

        let graph: GraphDef = serde_yaml::from_str(&format!(
            r#"
graph: file-ops
steps:
  - id: write
    tools:
      - {{ tool: "write_file", path: "{}/test_graph_output.txt", args: "hello from graph" }}
  - id: read
    depends_on: [write]
    tools:
      - {{ tool: "read_file", path: "{}/test_graph_output.txt" }}
"#,
            dir.display(),
            dir.display()
        ))
        .unwrap();

        let ctx = execute(&graph, &dir, None).await.unwrap();

        assert_eq!(
            ctx.results["read"].tool_results[0].stdout.trim(),
            "hello from graph"
        );

        // Cleanup.
        let _ = std::fs::remove_file(dir.join("test_graph_output.txt"));
        let _ = std::fs::remove_dir(&dir);
    }
}
