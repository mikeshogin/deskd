mod agent;
mod bus;
mod config;
mod message;
mod worker;

use clap::{Parser, Subcommand};
use tracing::info;

const DEFAULT_SOCKET: &str = "/tmp/deskd.sock";

#[derive(Parser)]
#[command(name = "deskd", about = "Agent orchestration runtime")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the message bus server and launch configured agents.
    Serve {
        /// Unix socket path for the bus.
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
        /// Path to workspace config file (workspace.yaml).
        /// When provided, agents listed in the config are auto-started.
        #[arg(long)]
        config: Option<String>,
    },
    /// Manage agents.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Register a new agent (saves state file, does not start worker).
    Create {
        /// Agent name.
        name: String,
        /// System prompt text.
        #[arg(long)]
        prompt: Option<String>,
        /// Claude model to use.
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        /// Working directory for claude.
        #[arg(long)]
        workdir: Option<String>,
        /// Max turns per task.
        #[arg(long, default_value = "100")]
        max_turns: u32,
        /// Linux user to run the agent process as (optional).
        #[arg(long)]
        unix_user: Option<String>,
        /// Budget cap in USD.
        #[arg(long, default_value = "50.0")]
        budget_usd: f64,
        /// Command to run as the agent process (default: claude).
        #[arg(long = "command")]
        command: Vec<String>,
    },
    /// Send a task to an agent (via bus if running, direct otherwise).
    Send {
        /// Agent name.
        name: String,
        /// Task message to send.
        message: String,
        /// Max turns for this task.
        #[arg(long)]
        max_turns: Option<u32>,
        /// Bus socket path.
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Start the worker loop for an agent (connect to bus, process tasks).
    Run {
        /// Agent name.
        name: String,
        /// Bus socket path.
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// List registered agents with their stats (and live status if bus is running).
    List {
        /// Bus socket path — when provided, shows which agents are currently connected.
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Show detailed stats for an agent.
    Stats {
        /// Agent name.
        name: String,
    },
    /// Remove an agent (state file + log).
    Rm {
        /// Agent name.
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { socket, config } => {
            serve(socket, config).await?;
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
                    name: name.clone(),
                    model,
                    system_prompt: prompt.unwrap_or_default(),
                    work_dir: workdir.unwrap_or_else(|| ".".into()),
                    max_turns,
                    unix_user,
                    budget_usd,
                    command: if command.is_empty() { vec!["claude".to_string()] } else { command },
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
                if std::path::Path::new(&socket).exists() {
                    let target = format!("agent:{}", name);
                    worker::send_via_bus(&socket, "cli", &target, &message, max_turns).await?;
                } else {
                    let response = agent::send(&name, &message, max_turns).await?;
                    println!("{}", response);
                }
            }
            AgentAction::Run { name, socket } => {
                agent::load_state(&name)?;
                info!(agent = %name, "starting worker");
                tokio::select! {
                    result = worker::run(&name, &socket) => { result?; }
                    _ = tokio::signal::ctrl_c() => {
                        info!(agent = %name, "shutting down");
                    }
                }
            }
            AgentAction::List { socket } => {
                let agents = agent::list().await?;
                // Query live connected agents from bus (best-effort).
                let live = query_live_agents(&socket).await.unwrap_or_default();

                if agents.is_empty() {
                    println!("No agents registered");
                } else {
                    println!(
                        "{:<15} {:<7} {:<8} {:<10} {:<12} {}",
                        "NAME", "STATUS", "TURNS", "COST", "USER", "MODEL"
                    );
                    for a in agents {
                        let status = if live.contains(&a.config.name) { "live" } else { "idle" };
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
                println!("Unix user:  {}", s.config.unix_user.as_deref().unwrap_or("-"));
                println!("Work dir:   {}", s.config.work_dir);
                println!("Total turns:{}", s.total_turns);
                println!("Total cost: ${:.4}", s.total_cost);
                println!("Budget:     ${:.2}", s.config.budget_usd);
                println!(
                    "Session:    {}",
                    if s.session_id.is_empty() { "-" } else { &s.session_id }
                );
                println!("Created:    {}", s.created_at);
            }
            AgentAction::Rm { name } => {
                agent::remove(&name).await?;
                println!("Agent {} removed", name);
            }
        },
    }

    Ok(())
}

async fn serve(socket: String, config_path: Option<String>) -> anyhow::Result<()> {
    let workspace = if let Some(path) = config_path {
        let ws = config::WorkspaceConfig::load(&path)?;
        info!(path = %path, agents = ws.agents.len(), "loaded workspace config");
        Some(ws)
    } else {
        None
    };

    // Workspace config overrides the CLI --socket flag.
    let effective_socket = workspace
        .as_ref()
        .map(|ws| ws.bus.socket.clone())
        .unwrap_or(socket);

    // Auto-spawn persistent agents defined in workspace config.
    if let Some(ref ws) = workspace {
        for def in &ws.agents {
            if !def.persistent {
                info!(agent = %def.name, "skipping non-persistent agent (on-demand only)");
                continue;
            }

            let state = agent::create_or_recover(def).await?;
            let name = state.config.name.clone();

            // Each persistent agent gets its own sub-bus for scoped sub-agent spawning.
            let sub_bus = def.sub_bus_path(&effective_socket);
            {
                let sub = sub_bus.clone();
                let agent_name = name.clone();
                tokio::spawn(async move {
                    if let Err(e) = bus::serve(&sub).await {
                        tracing::error!(agent = %agent_name, socket = %sub, error = %e, "sub-bus failed");
                    }
                });
            }
            info!(agent = %name, sub_bus = %sub_bus, "started sub-bus for agent");

            // Worker connects to the ROOT bus (receives tasks from external world).
            let sock = effective_socket.clone();
            tokio::spawn(async move {
                if let Err(e) = worker::run(&name, &sock).await {
                    tracing::error!(agent = %name, error = %e, "worker exited with error");
                }
            });
        }
    }

    info!(socket = %effective_socket, "starting root bus");
    tokio::select! {
        result = bus::serve(&effective_socket) => { result?; }
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
        }
    }

    Ok(())
}

/// Query the bus for currently connected agent names.
/// Returns empty vec if the bus is not running or unreachable.
async fn query_live_agents(socket_path: &str) -> anyhow::Result<std::collections::HashSet<String>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    if !std::path::Path::new(socket_path).exists() {
        return Ok(Default::default());
    }

    let mut stream = UnixStream::connect(socket_path).await?;

    // Register as transient query client.
    let reg = serde_json::json!({"type": "register", "name": "cli-list-query", "subscriptions": []});
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    // Send list query.
    let query = serde_json::json!({"type": "list"});
    let mut qline = serde_json::to_string(&query)?;
    qline.push('\n');
    stream.write_all(qline.as_bytes()).await?;

    // Read the response (one message).
    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let timeout = tokio::time::Duration::from_secs(2);
    let resp_line = tokio::time::timeout(timeout, lines.next_line()).await??;

    if let Some(line) = resp_line {
        let v: serde_json::Value = serde_json::from_str(&line)?;
        if let Some(arr) = v["payload"]["clients"].as_array() {
            return Ok(arr.iter()
                .filter_map(|c| c.as_str())
                .map(|s| s.to_string())
                .collect());
        }
    }

    Ok(Default::default())
}
