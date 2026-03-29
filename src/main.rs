mod acp;
mod adapters;
mod agent;
mod bus;
mod config;
pub mod context;
mod inbox;
mod mcp;
mod message;
mod schedule;
mod statemachine;
mod unified_inbox;
mod worker;
mod workflow;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tracing::info;

const DEFAULT_SOCKET: &str = "/tmp/deskd.sock";

fn version_string() -> &'static str {
    // Constructed once via a static; includes git hash when available.
    // Version comes from git tag (DESKD_VERSION) or falls back to Cargo.toml.
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        let ver = option_env!("DESKD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        let hash = env!("GIT_HASH");
        if hash.is_empty() {
            ver.to_string()
        } else {
            format!("{ver} ({hash})")
        }
    })
}

#[derive(Parser)]
#[command(name = "deskd", about = "Agent orchestration runtime", version = version_string())]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the bus and launch all persistent agents from workspace config.
    Serve {
        /// Path to workspace.yaml.
        #[arg(long)]
        config: String,
    },
    /// Run as MCP server for a specific agent (called by claude --mcp-server).
    /// Provides send_message and add_persistent_agent tools.
    Mcp {
        /// Agent name (must match an agent registered in deskd state).
        #[arg(long)]
        agent: String,
    },
    /// Manage agents.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Show live status of all agents defined in workspace config.
    Status {
        /// Path to workspace.yaml.
        #[arg(long)]
        config: String,
    },
    /// Kill the running `deskd serve` process and restart it with the same config.
    Restart {
        /// Path to workspace.yaml. Required if the running process cannot be auto-detected.
        #[arg(long)]
        config: Option<String>,
    },
    /// Download the latest deskd release binary, replace the current installation,
    /// then restart deskd serve if it is running.
    Upgrade {
        /// Install directory. Defaults to the directory of the current binary,
        /// falling back to ~/.local/bin.
        #[arg(long)]
        install_dir: Option<String>,
    },
    /// State machine: manage models and instances.
    Sm {
        /// Path to deskd.yaml with model definitions.
        #[arg(long, env = "DESKD_AGENT_CONFIG")]
        config: String,
        #[command(subcommand)]
        action: SmAction,
    },
    /// Schedule a one-shot reminder for an agent.
    ///
    /// Writes a RemindDef JSON to ~/.deskd/reminders/<uuid>.json.
    /// The reminder runner (part of `deskd serve`) will fire it when due.
    ///
    /// Examples:
    ///   deskd remind kira --in 30m "Check PR status"
    ///   deskd remind kira --in 2h30m "Stand-up time"
    ///   deskd remind kira --at 2026-03-27T15:00:00Z "Deploy window opens"
    ///   deskd remind kira --in 1h --target queue:reviews "Review queue check"
    Remind {
        /// Agent name. Used as bus target `agent:<name>` unless --target is given.
        name: String,
        /// Duration from now (e.g. 30m, 1h, 2h30m, 90s). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        r#in: Option<String>,
        /// Absolute ISO 8601 timestamp. Mutually exclusive with --in.
        #[arg(long, conflicts_with = "in")]
        at: Option<String>,
        /// Override bus target (default: agent:<name>).
        #[arg(long)]
        target: Option<String>,
        /// Message payload to deliver.
        message: String,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Register a new agent (saves state file, does not start worker).
    Create {
        name: String,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long)]
        workdir: Option<String>,
        #[arg(long, default_value = "100")]
        max_turns: u32,
        #[arg(long)]
        unix_user: Option<String>,
        #[arg(long, default_value = "50.0")]
        budget_usd: f64,
        #[arg(long, num_args = 1.., value_delimiter = ' ')]
        command: Vec<String>,
    },
    /// Send a task to an agent (via bus if running, or directly).
    Send {
        name: String,
        message: String,
        #[arg(long)]
        max_turns: Option<u32>,
        /// Bus socket path. When omitted, resolved from agent state file
        /// (~/.deskd/agents/<name>.yaml → {work_dir}/.deskd/bus.sock).
        #[arg(long)]
        socket: Option<String>,
    },
    /// Start the worker loop for an agent (connect to bus, process tasks).
    Run {
        name: String,
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
        /// Custom subscriptions (overrides defaults). Can be repeated.
        #[arg(long)]
        subscribe: Vec<String>,
    },
    /// List registered agents with live status.
    List {
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Show detailed stats for an agent.
    Stats { name: String },
    /// Read buffered task results from an agent's inbox.
    Read {
        name: String,
        /// Remove messages after reading.
        #[arg(long, default_value = "false")]
        clear: bool,
        /// Keep watching for new messages after printing existing ones.
        #[arg(long, default_value = "false")]
        follow: bool,
    },
    /// Show recent completed tasks for an agent (from inbox files).
    Tasks {
        /// Agent name, or "all" to show tasks for all agents.
        name: String,
        /// Show last N tasks (default 20).
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Remove an agent (state file + log).
    Rm { name: String },
    /// Spawn an ephemeral sub-agent, run a task, print result, clean up.
    Spawn {
        name: String,
        task: String,
        /// Bus socket (defaults to $DESKD_BUS_SOCKET).
        #[arg(long)]
        socket: Option<String>,
        #[arg(long)]
        work_dir: Option<String>,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long, default_value = "50")]
        max_turns: u32,
    },
}

#[derive(Subcommand)]
enum SmAction {
    /// List defined models.
    Models,
    /// Show a model's states and transitions.
    Show { model: String },
    /// Create a new instance of a model.
    Create {
        model: String,
        title: String,
        #[arg(long)]
        body: Option<String>,
    },
    /// Move an instance to a new state.
    Move {
        id: String,
        state: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Show instance details and history.
    Status { id: String },
    /// List instances, optionally filtered.
    List {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Cancel an instance (move to terminal state if available).
    Cancel { id: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            config: config_path,
        } => {
            serve(config_path).await?;
        }
        Commands::Mcp { agent } => {
            mcp::run(&agent).await?;
        }
        Commands::Agent { action } => match action {
            AgentAction::Create {
                name,
                prompt,
                model,
                workdir,
                max_turns,
                unix_user,
                budget_usd,
                command,
            } => {
                let cfg = agent::AgentConfig {
                    name,
                    model,
                    system_prompt: prompt.unwrap_or_default(),
                    work_dir: workdir.unwrap_or_else(|| ".".into()),
                    max_turns,
                    unix_user,
                    budget_usd,
                    command: if command.is_empty() {
                        vec!["claude".to_string()]
                    } else {
                        command
                    },
                    config_path: None,
                    container: None,
                    session: config::SessionMode::default(),
                    runtime: config::AgentRuntime::default(),
                };
                let state = agent::create(&cfg).await?;
                println!("Agent {} created", state.config.name);
            }
            AgentAction::Send {
                name,
                message,
                max_turns,
                socket,
            } => {
                // Socket priority: explicit --socket > agent's bus from state > direct exec.
                let effective_socket = if let Some(ref s) = socket {
                    if std::path::Path::new(s).exists() {
                        socket
                    } else {
                        None
                    }
                } else {
                    agent::load_state(&name).ok().and_then(|s| {
                        let bus = config::agent_bus_socket(&s.config.work_dir);
                        if std::path::Path::new(&bus).exists() {
                            Some(bus)
                        } else {
                            None
                        }
                    })
                };

                if let Some(sock) = effective_socket {
                    let target = format!("agent:{}", name);
                    worker::send_via_bus(&sock, "cli", &target, &message, max_turns).await?;
                } else {
                    let response = agent::send(&name, &message, max_turns, None).await?;
                    println!("{}", response);
                }
            }
            AgentAction::Run {
                name,
                socket,
                subscribe,
            } => {
                agent::load_state(&name)?;
                let subs = if subscribe.is_empty() {
                    None
                } else {
                    Some(subscribe)
                };
                info!(agent = %name, "starting worker");
                tokio::select! {
                    result = worker::run(&name, &socket, Some(socket.clone()), subs) => { result?; }
                    _ = tokio::signal::ctrl_c() => {
                        info!(agent = %name, "shutting down");
                    }
                }
            }
            AgentAction::List { socket } => {
                let agents = agent::list().await?;
                let live = query_live_agents(&socket).await.unwrap_or_default();

                if agents.is_empty() {
                    println!("No agents registered");
                } else {
                    println!(
                        "{:<15} {:<7} {:<8} {:<10} {:<12} MODEL",
                        "NAME", "STATUS", "TURNS", "COST", "USER"
                    );
                    for a in agents {
                        let status = if live.contains(&a.config.name) {
                            "live"
                        } else {
                            "idle"
                        };
                        println!(
                            "{:<15} {:<7} {:<8} ${:<9.2} {:<12} {}",
                            a.config.name,
                            status,
                            a.total_turns,
                            a.total_cost,
                            a.config.unix_user.as_deref().unwrap_or("-"),
                            a.config.model,
                        );
                    }
                }
            }
            AgentAction::Stats { name } => {
                let s = agent::load_state(&name)?;
                println!("Agent:      {}", s.config.name);
                println!("Model:      {}", s.config.model);
                println!(
                    "Unix user:  {}",
                    s.config.unix_user.as_deref().unwrap_or("-")
                );
                println!("Work dir:   {}", s.config.work_dir);
                println!(
                    "Bus:        {}",
                    config::agent_bus_socket(&s.config.work_dir)
                );
                println!(
                    "Config:     {}",
                    s.config.config_path.as_deref().unwrap_or("-")
                );
                println!("Total turns:{}", s.total_turns);
                println!("Total cost: ${:.4}", s.total_cost);
                println!("Budget:     ${:.2}", s.config.budget_usd);
                println!(
                    "Session:    {}",
                    if s.session_id.is_empty() {
                        "-"
                    } else {
                        &s.session_id
                    }
                );
                println!("Created:    {}", s.created_at);
            }
            AgentAction::Read {
                name,
                clear,
                follow,
            } => {
                let entries = inbox::read(&name)?;
                if entries.is_empty() && !follow {
                    println!("No messages for {}", name);
                } else {
                    let paths: Vec<_> = entries.iter().map(|(p, _)| p.clone()).collect();
                    for (_, entry) in &entries {
                        print_inbox_entry(entry);
                    }
                    if clear {
                        inbox::clear(&paths)?;
                        println!("({} message(s) cleared)", paths.len());
                    }
                }
                if follow {
                    let mut seen: std::collections::HashSet<std::path::PathBuf> =
                        inbox::read(&name)?.into_iter().map(|(p, _)| p).collect();
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        let current = inbox::read(&name)?;
                        for (path, entry) in &current {
                            if !seen.contains(path) {
                                seen.insert(path.clone());
                                print_inbox_entry(entry);
                            }
                        }
                    }
                }
            }
            AgentAction::Tasks { name, limit } => {
                let all_entries = inbox::read_all()?;
                let show_all = name == "all";
                let mut filtered: Vec<_> = if show_all {
                    all_entries
                } else {
                    all_entries
                        .into_iter()
                        .filter(|e| e.agent == name)
                        .collect()
                };
                // Sort by timestamp descending (newest first).
                filtered.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                filtered.truncate(limit);

                if filtered.is_empty() {
                    if show_all {
                        println!("No completed tasks found");
                    } else {
                        println!("No completed tasks for {}", name);
                    }
                } else {
                    println!("COMPLETED ({}):", filtered.len());
                    let now = chrono::Utc::now();
                    for entry in &filtered {
                        let age = if let Ok(ts) =
                            chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                        {
                            let dur = now.signed_duration_since(ts);
                            format_relative_time(dur)
                        } else {
                            "??".to_string()
                        };
                        let id_short = if entry.id.len() > 6 {
                            &entry.id[..6]
                        } else {
                            &entry.id
                        };
                        let task_excerpt = truncate_main(&entry.task, 36);
                        let status = if entry.error.is_some() { "err" } else { "done" };
                        if show_all {
                            println!(
                                "  {:<8} {:<12} from:{:<6} {:38} {} {} ago",
                                id_short,
                                entry.agent,
                                entry.source,
                                format!("\"{}\"", task_excerpt),
                                status,
                                age,
                            );
                        } else {
                            println!(
                                "  {:<8} from:{:<6} {:38} {} {} ago",
                                id_short,
                                entry.source,
                                format!("\"{}\"", task_excerpt),
                                status,
                                age,
                            );
                        }
                    }
                }
            }
            AgentAction::Rm { name } => {
                agent::remove(&name).await?;
                println!("Agent {} removed", name);
            }
            AgentAction::Spawn {
                name,
                task,
                socket,
                work_dir,
                model,
                max_turns,
            } => {
                let bus_socket = socket
                    .or_else(|| std::env::var("DESKD_BUS_SOCKET").ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!("No bus socket: pass --socket or set DESKD_BUS_SOCKET")
                    })?;

                let parent = std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "unknown".into());
                let resolved_work_dir = work_dir.unwrap_or_else(|| ".".into());

                let response = agent::spawn_ephemeral(
                    &name,
                    &task,
                    &model,
                    &resolved_work_dir,
                    max_turns,
                    &bus_socket,
                    &parent,
                )
                .await?;

                println!("{}", response);
            }
        },
        Commands::Sm {
            config: config_path,
            action,
        } => {
            let user_cfg = config::UserConfig::load(&config_path)?;
            handle_sm(action, &user_cfg)?;
        }
        Commands::Remind {
            name,
            r#in: duration_str,
            at,
            target,
            message,
        } => {
            handle_remind(name, duration_str, at, target, message)?;
        }
        Commands::Status {
            config: config_path,
        } => {
            let workspace = config::WorkspaceConfig::load(&config_path)?;
            println!(
                "{:<12} {:<9} {:<6} {:<10} CURRENT TASK",
                "NAME", "STATUS", "TURNS", "COST"
            );
            println!("{}", "─".repeat(70));
            for def in &workspace.agents {
                let bus_socket = def.bus_socket();
                let online = std::path::Path::new(&bus_socket).exists();
                let state = agent::load_state(&def.name).ok();
                let (status, turns, cost, current_task) = match &state {
                    Some(s) if online => (
                        s.status.as_str(),
                        s.total_turns,
                        s.total_cost,
                        if s.current_task.is_empty() {
                            "-".to_string()
                        } else {
                            truncate_main(&s.current_task, 40)
                        },
                    ),
                    Some(s) => ("offline", s.total_turns, s.total_cost, "-".to_string()),
                    None => ("offline", 0, 0.0, "-".to_string()),
                };
                println!(
                    "{:<12} {:<9} {:<6} ${:<9.4} {}",
                    def.name, status, turns, cost, current_task
                );
            }
        }
        Commands::Restart { config } => {
            restart(config).await?;
        }
        Commands::Upgrade { install_dir } => {
            upgrade(install_dir).await?;
        }
    }

    Ok(())
}

fn handle_sm(action: SmAction, user_cfg: &config::UserConfig) -> anyhow::Result<()> {
    let store = statemachine::StateMachineStore::default_for_home();
    match action {
        SmAction::Models => {
            if user_cfg.models.is_empty() {
                println!("No models defined");
            } else {
                println!("{:<20} {:<8} {:<8} DESCRIPTION", "NAME", "STATES", "TRANS");
                for m in &user_cfg.models {
                    println!(
                        "{:<20} {:<8} {:<8} {}",
                        m.name,
                        m.states.len(),
                        m.transitions.len(),
                        m.description,
                    );
                }
            }
        }
        SmAction::Show { model } => {
            let m = user_cfg
                .models
                .iter()
                .find(|m| m.name == model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model))?;
            println!("Model:    {}", m.name);
            if !m.description.is_empty() {
                println!("Desc:     {}", m.description);
            }
            println!("States:   {}", m.states.join(", "));
            println!("Initial:  {}", m.initial);
            println!(
                "Terminal: {}",
                if m.terminal.is_empty() {
                    "-".to_string()
                } else {
                    m.terminal.join(", ")
                }
            );
            println!();
            println!(
                "{:<15} {:<15} {:<12} {:<12} ASSIGNEE",
                "FROM", "TO", "TRIGGER", "ON"
            );
            for t in &m.transitions {
                println!(
                    "{:<15} {:<15} {:<12} {:<12} {}",
                    t.from,
                    t.to,
                    t.trigger.as_deref().unwrap_or("-"),
                    t.on.as_deref().unwrap_or("-"),
                    t.assignee.as_deref().unwrap_or("-"),
                );
            }
        }
        SmAction::Create { model, title, body } => {
            let m = user_cfg
                .models
                .iter()
                .find(|m| m.name == model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model))?;
            let creator = std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "cli".to_string());
            let inst = store.create(m, &title, body.as_deref().unwrap_or(""), &creator)?;
            println!(
                "Created {} (model={}, state={})",
                inst.id, inst.model, inst.state
            );
        }
        SmAction::Move { id, state, note } => {
            let mut inst = store.load(&id)?;
            let m = user_cfg
                .models
                .iter()
                .find(|m| m.name == inst.model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found in config", inst.model))?;
            let trigger =
                std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "manual".to_string());
            store.move_to(&mut inst, m, &state, &trigger, note.as_deref())?;
            println!("{} -> {} ({})", id, inst.state, inst.model);
        }
        SmAction::Status { id } => {
            let inst = store.load(&id)?;
            println!("ID:        {}", inst.id);
            println!("Model:     {}", inst.model);
            println!("Title:     {}", inst.title);
            if !inst.body.is_empty() {
                println!("Body:      {}", inst.body);
            }
            println!("State:     {}", inst.state);
            println!("Assignee:  {}", inst.assignee);
            if let Some(ref r) = inst.result {
                println!("Result:    {}", r);
            }
            if let Some(ref e) = inst.error {
                println!("Error:     {}", e);
            }
            println!("Created:   {} by {}", inst.created_at, inst.created_by);
            println!("Updated:   {}", inst.updated_at);
            if !inst.history.is_empty() {
                println!();
                println!("{:<15} {:<15} {:<20} TIMESTAMP", "FROM", "TO", "TRIGGER");
                for h in &inst.history {
                    println!(
                        "{:<15} {:<15} {:<20} {}",
                        h.from, h.to, h.trigger, h.timestamp,
                    );
                }
            }
        }
        SmAction::List {
            model,
            state,
            limit,
        } => {
            let mut instances = store.list_all()?;
            if let Some(ref m) = model {
                instances.retain(|i| i.model == *m);
            }
            if let Some(ref s) = state {
                instances.retain(|i| i.state == *s);
            }
            instances.truncate(limit);
            if instances.is_empty() {
                println!("No instances found");
            } else {
                println!(
                    "{:<12} {:<15} {:<12} {:<12} TITLE",
                    "ID", "MODEL", "STATE", "ASSIGNEE"
                );
                for inst in &instances {
                    println!(
                        "{:<12} {:<15} {:<12} {:<12} {}",
                        inst.id,
                        inst.model,
                        inst.state,
                        inst.assignee,
                        truncate_main(&inst.title, 40),
                    );
                }
            }
        }
        SmAction::Cancel { id } => {
            let mut inst = store.load(&id)?;
            let m = user_cfg
                .models
                .iter()
                .find(|m| m.name == inst.model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found in config", inst.model))?;
            if statemachine::is_terminal(m, &inst) {
                println!("{} is already in terminal state '{}'", id, inst.state);
                return Ok(());
            }
            // Find a transition to a terminal state from current state.
            let valid = statemachine::valid_transitions(m, &inst.state);
            let cancel_target = valid
                .iter()
                .find(|t| m.terminal.contains(&t.to))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No transition to a terminal state from '{}' in model '{}'",
                        inst.state,
                        m.name
                    )
                })?;
            let target = cancel_target.to.clone();
            store.move_to(&mut inst, m, &target, "cancel", Some("Cancelled via CLI"))?;
            println!("{} cancelled -> {}", id, inst.state);
        }
    }
    Ok(())
}

fn print_inbox_entry(entry: &inbox::InboxEntry) {
    println!(
        "─── {} → {} [{}] ───",
        entry.source,
        entry.agent,
        &entry.timestamp[..19.min(entry.timestamp.len())]
    );
    println!("Task: {}", truncate_main(&entry.task, 120));
    if let Some(ref result) = entry.result {
        println!("{}", result);
    }
    if let Some(ref error) = entry.error {
        println!("ERROR: {}", error);
    }
    println!();
}

fn truncate_main(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Find a running `deskd serve` process by scanning /proc (Linux only).
/// Returns (pid, config_path) of the first match found.
#[cfg(target_os = "linux")]
fn find_serve_process() -> Option<(u32, String)> {
    use std::fs;
    let proc = fs::read_dir("/proc").ok()?;
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid == std::process::id() {
            continue;
        }
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        let Ok(cmdline) = fs::read(&cmdline_path) else {
            continue;
        };
        // cmdline entries are NUL-separated
        let args: Vec<&str> = cmdline
            .split(|&b| b == 0)
            .filter_map(|s| {
                let s = std::str::from_utf8(s).ok()?;
                if s.is_empty() { None } else { Some(s) }
            })
            .collect();

        let is_deskd = args.first().map(|a| a.ends_with("deskd")).unwrap_or(false);
        let has_serve = args.contains(&"serve");
        if is_deskd && has_serve {
            let config = args.windows(2).find_map(|w| {
                if w[0] == "--config" {
                    Some(w[1].to_string())
                } else {
                    None
                }
            });
            return Some((pid, config.unwrap_or_default()));
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn find_serve_process() -> Option<(u32, String)> {
    None
}

/// Kill the running `deskd serve` and restart it with the same (or provided) config.
async fn restart(config_override: Option<String>) -> anyhow::Result<()> {
    use std::time::Duration;

    let (pid, detected_config) = match find_serve_process() {
        Some(p) => p,
        None => {
            if let Some(cfg) = config_override {
                info!("No running deskd serve found — starting fresh with {}", cfg);
                serve(cfg).await?;
                return Ok(());
            }
            anyhow::bail!(
                "No running `deskd serve` process found. \
                 Pass --config to start one, or start it manually first."
            );
        }
    };

    let config_path = config_override
        .or_else(|| {
            if detected_config.is_empty() {
                None
            } else {
                Some(detected_config.clone())
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Found deskd serve (pid {}) but could not determine its --config path. \
                 Pass --config explicitly.",
                pid
            )
        })?;

    info!(pid = pid, config = %config_path, "sending SIGTERM to deskd serve");
    let kill_status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run kill: {}", e))?;
    if !kill_status.success() {
        anyhow::bail!("kill -TERM {} failed: {}", pid, kill_status);
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !std::path::Path::new(&format!("/proc/{}", pid)).exists() {
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "deskd serve (pid {}) did not exit within 10 s after SIGTERM",
                pid
            );
        }
    }
    info!(pid = pid, "old deskd serve exited — restarting");

    serve(config_path).await
}

/// Download the latest deskd binary from GitHub Releases and replace the current binary.
/// Restarts deskd serve if it is running.
async fn upgrade(install_dir_override: Option<String>) -> anyhow::Result<()> {
    let install_dir = if let Some(dir) = install_dir_override {
        std::path::PathBuf::from(dir)
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                std::path::PathBuf::from(home).join(".local").join("bin")
            })
    };

    let os_name = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => anyhow::bail!("Unsupported OS for upgrade: {}", other),
    };
    let arch_name = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => anyhow::bail!("Unsupported architecture for upgrade: {}", other),
    };

    let artifact = format!("deskd-{}-{}", os_name, arch_name);
    let url = format!(
        "https://github.com/kgatilin/deskd/releases/latest/download/{}",
        artifact
    );

    println!("Downloading {} ...", url);
    std::fs::create_dir_all(&install_dir)?;

    let tmp_path = install_dir.join(".deskd-upgrade.tmp");
    let dest_path = install_dir.join("deskd");

    let status = std::process::Command::new("curl")
        .args([
            "-fsSL",
            &url,
            "-o",
            tmp_path.to_str().unwrap_or("/tmp/.deskd-upgrade.tmp"),
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run curl: {}", e))?;

    if !status.success() {
        anyhow::bail!(
            "curl download failed (exit {}). Check https://github.com/kgatilin/deskd/releases",
            status
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    std::fs::rename(&tmp_path, &dest_path)?;
    println!("Installed: {}", dest_path.display());

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("codesign")
            .args([
                "--force",
                "--sign",
                "-",
                dest_path.to_str().unwrap_or("deskd"),
            ])
            .status();
    }

    match find_serve_process() {
        Some((pid, ref config_path)) if !config_path.is_empty() => {
            println!(
                "Restarting deskd serve (pid {}) with config {}",
                pid, config_path
            );
            restart(Some(config_path.clone())).await?;
        }
        Some((pid, _)) => {
            println!(
                "deskd serve (pid {}) is running but config path is unknown — \
                 please restart it manually.",
                pid
            );
        }
        None => {
            println!("No running deskd serve detected — upgrade complete.");
        }
    }

    Ok(())
}

/// Parse a simple duration string into total seconds.
///
/// Supported formats:
///   `30m`    → 1800 seconds
///   `1h`     → 3600 seconds
///   `2h30m`  → 9000 seconds
///   `90s`    → 90 seconds
///   Combined forms: `1h30m`, `2h15m30s`, etc.
fn parse_duration_secs(s: &str) -> anyhow::Result<u64> {
    let mut total: u64 = 0;
    let mut current_num = String::new();
    let mut found_any = false;

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current_num.push(ch);
        } else {
            let n: u64 = if current_num.is_empty() {
                anyhow::bail!("expected number before '{}' in duration '{}'", ch, s)
            } else {
                current_num
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid number in duration '{}'", s))?
            };
            current_num.clear();

            match ch {
                'h' => {
                    total += n * 3600;
                    found_any = true;
                }
                'm' => {
                    total += n * 60;
                    found_any = true;
                }
                's' => {
                    total += n;
                    found_any = true;
                }
                other => {
                    anyhow::bail!("unknown unit '{}' in duration '{}' (use h, m, s)", other, s)
                }
            }
        }
    }

    if !current_num.is_empty() {
        anyhow::bail!(
            "trailing number '{}' without unit in duration '{}' (use h, m, s)",
            current_num,
            s
        );
    }
    if !found_any {
        anyhow::bail!(
            "empty or invalid duration '{}' — expected e.g. 30m, 1h, 2h30m, 90s",
            s
        );
    }

    Ok(total)
}

/// Handle `deskd remind <name> [--in <dur> | --at <ts>] [--target <t>] "<message>"`.
fn handle_remind(
    name: String,
    duration_str: Option<String>,
    at: Option<String>,
    target_override: Option<String>,
    message: String,
) -> anyhow::Result<()> {
    let fire_at: chrono::DateTime<chrono::Utc> = if let Some(ref dur) = duration_str {
        let secs = parse_duration_secs(dur)?;
        chrono::Utc::now() + chrono::Duration::seconds(secs as i64)
    } else if let Some(ref ts) = at {
        chrono::DateTime::parse_from_rfc3339(ts)
            .with_context(|| format!("invalid --at timestamp '{}' (expected ISO 8601)", ts))?
            .with_timezone(&chrono::Utc)
    } else {
        anyhow::bail!("either --in <duration> or --at <timestamp> is required");
    };

    let target = target_override.unwrap_or_else(|| format!("agent:{}", name));

    let remind = config::RemindDef {
        at: fire_at.to_rfc3339(),
        target: target.clone(),
        message,
    };

    let dir = config::reminders_dir();
    let filename = format!("{}.json", uuid::Uuid::new_v4());
    let path = dir.join(&filename);

    let json = serde_json::to_string_pretty(&remind).context("failed to serialize reminder")?;
    std::fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;

    println!(
        "Reminder scheduled: target={} at={} file={}",
        target,
        fire_at.to_rfc3339(),
        path.display()
    );

    Ok(())
}

/// Format a chrono::Duration as a human-readable relative time string (e.g. "5m", "2h", "3d").
fn format_relative_time(dur: chrono::Duration) -> String {
    let secs = dur.num_seconds();
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Start per-agent buses and workers for all agents in workspace config.
/// Each agent has its own isolated bus at {work_dir}/.deskd/bus.sock.
/// No shared root bus.
async fn serve(config_path: String) -> anyhow::Result<()> {
    let workspace = config::WorkspaceConfig::load(&config_path)?;
    info!(path = %config_path, agents = workspace.agents.len(), "loaded workspace config");

    if workspace.agents.is_empty() {
        tracing::warn!("No agents defined in workspace config");
    }

    for def in &workspace.agents {
        let cfg_path = def.config_path();
        let user_cfg = config::UserConfig::load(&cfg_path).ok();
        if user_cfg.is_some() {
            info!(agent = %def.name, config = %cfg_path, "loaded user config");
        } else {
            info!(agent = %def.name, "no user config at {}, using defaults", cfg_path);
        }

        let state = agent::create_or_recover(def, user_cfg.as_ref()).await?;
        let name = state.config.name.clone();
        let bus_socket = def.bus_socket();

        // Ensure {work_dir}/.deskd/ exists.
        let bus_dir = std::path::Path::new(&def.work_dir).join(".deskd");
        std::fs::create_dir_all(&bus_dir)?;

        // Start the agent's isolated bus.
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            tokio::spawn(async move {
                if let Err(e) = bus::serve(&bus).await {
                    tracing::error!(agent = %agent_name, socket = %bus, error = %e, "bus failed");
                }
            });
        }
        info!(agent = %name, bus = %bus_socket, "started agent bus");

        // Start configured adapters (Telegram, Discord, etc.).
        for adapter in adapters::build_adapters(def, user_cfg.as_ref()) {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            let adapter_name = adapter.name().to_string();
            tokio::spawn(async move {
                if let Err(e) = adapter.run(bus, agent_name).await {
                    tracing::error!(adapter = %adapter_name, error = %e, "adapter failed");
                }
            });
        }

        // Start schedule watcher — handles initial load + hot-reload on config changes.
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            let config = cfg_path.clone();
            let home = def.work_dir.clone();
            tokio::spawn(async move {
                schedule::watch_and_reload(config, bus, agent_name, home).await;
            });
            info!(agent = %name, "started schedule watcher");
        }

        // Start reminder runner — fires one-shot reminders from ~/.deskd/reminders/.
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            tokio::spawn(async move {
                schedule::run_reminders(bus, agent_name).await;
            });
            info!(agent = %name, "started reminder runner");
        }

        // Start worker on the agent's bus.
        let bus = bus_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = worker::run(&name, &bus, Some(bus.clone()), None).await {
                tracing::error!(agent = %name, error = %e, "worker exited with error");
            }
        });

        // Start sub-agent workers defined in the agent's deskd.yaml.
        if let Some(ref ucfg) = user_cfg {
            for sub in &ucfg.agents {
                let mcp_json = serde_json::json!({
                    "mcpServers": {
                        "deskd": {
                            "command": "deskd",
                            "args": ["mcp", "--agent", &sub.name]
                        }
                    }
                })
                .to_string();

                let sub_cfg = agent::AgentConfig {
                    name: sub.name.clone(),
                    model: sub.model.clone(),
                    system_prompt: sub.system_prompt.clone(),
                    work_dir: def.work_dir.clone(),
                    max_turns: ucfg.max_turns,
                    unix_user: def.unix_user.clone(),
                    budget_usd: def.budget_usd,
                    command: vec![
                        "claude".into(),
                        "--output-format".into(),
                        "stream-json".into(),
                        "--verbose".into(),
                        "--dangerously-skip-permissions".into(),
                        "--model".into(),
                        sub.model.clone(),
                        "--max-turns".into(),
                        ucfg.max_turns.to_string(),
                        "--mcp-config".into(),
                        mcp_json,
                    ],
                    config_path: Some(cfg_path.clone()),
                    container: def.container.clone(),
                    session: sub.session.clone(),
                    runtime: sub.runtime.clone(),
                };
                agent::create_or_update_from_config(&sub_cfg).await?;

                let sub_name = sub.name.clone();
                let bus = bus_socket.clone();
                let subs = sub.subscribe.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        worker::run(&sub_name, &bus, Some(bus.clone()), Some(subs)).await
                    {
                        tracing::error!(agent = %sub_name, error = %e, "sub-agent worker exited");
                    }
                });
                info!(agent = %def.name, sub_agent = %sub.name, "started sub-agent worker");
            }
        }

        // Start workflow engine if models are defined.
        if let Some(ref ucfg) = user_cfg
            && !ucfg.models.is_empty()
        {
            let bus = bus_socket.clone();
            let models = ucfg.models.clone();
            let agent_name = def.name.clone();
            tokio::spawn(async move {
                if let Err(e) = workflow::run(&bus, models).await {
                    tracing::error!(agent = %agent_name, error = %e, "workflow engine exited");
                }
            });
            info!(agent = %def.name, models = ucfg.models.len(), "started workflow engine");
        }
    }

    info!("all agents started — press Ctrl-C to stop");
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}

/// Query which agents are currently connected to a bus socket.
async fn query_live_agents(socket: &str) -> anyhow::Result<std::collections::HashSet<String>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    if !std::path::Path::new(socket).exists() {
        return Ok(Default::default());
    }

    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("connect: {}", e))?;

    let reg =
        serde_json::json!({"type": "register", "name": "deskd-cli-list", "subscriptions": []});
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let list_req = serde_json::json!({"type": "list"});
    let mut req_line = serde_json::to_string(&list_req)?;
    req_line.push('\n');
    stream.write_all(req_line.as_bytes()).await?;

    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let timeout = tokio::time::Duration::from_secs(2);
    let result = tokio::time::timeout(timeout, async {
        while let Some(l) = lines.next_line().await? {
            let v: serde_json::Value = serde_json::from_str(&l)?;
            if v.get("type").and_then(|t| t.as_str()) == Some("list_response")
                && let Some(arr) = v.get("clients").and_then(|c| c.as_array())
            {
                return Ok::<_, anyhow::Error>(
                    arr.iter()
                        .filter_map(|n| n.as_str())
                        .map(|s| s.to_string())
                        .collect(),
                );
            }
        }
        Ok(Default::default())
    })
    .await;

    result.unwrap_or(Ok(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration_secs("90s").unwrap(), 90);
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration_secs("30m").unwrap(), 30 * 60);
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration_secs("1h").unwrap(), 3600);
    }

    #[test]
    fn test_parse_duration_combined() {
        assert_eq!(parse_duration_secs("2h30m").unwrap(), 2 * 3600 + 30 * 60);
    }

    #[test]
    fn test_parse_duration_full() {
        assert_eq!(
            parse_duration_secs("1h15m30s").unwrap(),
            3600 + 15 * 60 + 30
        );
    }

    #[test]
    fn test_parse_duration_invalid_unit() {
        assert!(parse_duration_secs("5d").is_err());
    }

    #[test]
    fn test_parse_duration_empty() {
        assert!(parse_duration_secs("").is_err());
    }

    #[test]
    fn test_parse_duration_trailing_number() {
        assert!(parse_duration_secs("30").is_err());
    }

    #[test]
    fn test_handle_remind_writes_file() {
        // Use a unique temp directory under /tmp to avoid polluting ~/.deskd.
        let tmp_dir = std::path::PathBuf::from(format!(
            "/tmp/deskd-test-remind-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        // Override HOME so reminders_dir() writes to our temp dir.
        unsafe {
            std::env::set_var("HOME", &tmp_dir);
        }

        handle_remind(
            "kira".to_string(),
            Some("30m".to_string()),
            None,
            None,
            "test reminder".to_string(),
        )
        .unwrap();

        let remind_dir = tmp_dir.join(".deskd").join("reminders");
        let files: Vec<_> = std::fs::read_dir(&remind_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();

        assert_eq!(files.len(), 1, "expected exactly one reminder file");

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: config::RemindDef = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.target, "agent:kira");
        assert_eq!(parsed.message, "test reminder");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
