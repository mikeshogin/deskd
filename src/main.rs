mod adapters;
mod agent;
mod bus;
mod config;
mod mcp;
mod message;
mod schedule;
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
        /// Bus socket path. Defaults to agent's bus derived from state.
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Start the worker loop for an agent (connect to bus, process tasks).
    Run {
        name: String,
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// List registered agents with live status.
    List {
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Show detailed stats for an agent.
    Stats {
        name: String,
    },
    /// Remove an agent (state file + log).
    Rm {
        name: String,
    },
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
        Commands::Serve { config: config_path } => {
            serve(config_path).await?;
        }
        Commands::Mcp { agent } => {
            mcp::run(&agent).await?;
        }
        Commands::Agent { action } => match action {
            AgentAction::Create {
                name, prompt, model, workdir, max_turns, unix_user, budget_usd, command,
            } => {
                let cfg = agent::AgentConfig {
                    name,
                    model,
                    system_prompt: prompt.unwrap_or_default(),
                    work_dir: workdir.unwrap_or_else(|| ".".into()),
                    max_turns,
                    unix_user,
                    budget_usd,
                    command: if command.is_empty() { vec!["claude".to_string()] } else { command },
                    config_path: None,
                };
                let state = agent::create(&cfg).await?;
                println!("Agent {} created", state.config.name);
            }
            AgentAction::Send { name, message, max_turns, socket } => {
                // Socket priority: explicit --socket (if exists) > agent's bus from state > direct exec.
                let effective_socket = if std::path::Path::new(&socket).exists() {
                    Some(socket)
                } else {
                    agent::load_state(&name).ok().and_then(|s| {
                        let bus = config::agent_bus_socket(&s.config.work_dir);
                        if std::path::Path::new(&bus).exists() { Some(bus) } else { None }
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
            AgentAction::Run { name, socket } => {
                agent::load_state(&name)?;
                info!(agent = %name, "starting worker");
                tokio::select! {
                    result = worker::run(&name, &socket, Some(socket.clone())) => { result?; }
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
                        "{:<15} {:<7} {:<8} {:<10} {:<12} {}",
                        "NAME", "STATUS", "TURNS", "COST", "USER", "MODEL"
                    );
                    for a in agents {
                        let status = if live.contains(&a.config.name) { "live" } else { "idle" };
                        println!(
                            "{:<15} {:<7} {:<8} ${:<9.2} {:<12} {}",
                            a.config.name, status, a.total_turns, a.total_cost,
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
                println!("Bus:        {}", config::agent_bus_socket(&s.config.work_dir));
                println!("Config:     {}", s.config.config_path.as_deref().unwrap_or("-"));
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
            AgentAction::Spawn { name, task, socket, work_dir, model, max_turns } => {
                let bus_socket = socket
                    .or_else(|| std::env::var("DESKD_BUS_SOCKET").ok())
                    .ok_or_else(|| anyhow::anyhow!(
                        "No bus socket: pass --socket or set DESKD_BUS_SOCKET"
                    ))?;

                let parent = std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "unknown".into());
                let resolved_work_dir = work_dir.unwrap_or_else(|| ".".into());

                let response = agent::spawn_ephemeral(
                    &name, &task, &model, &resolved_work_dir, max_turns, &bus_socket, &parent,
                ).await?;

                println!("{}", response);
            }
        },
    }

    Ok(())
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

        // Start Telegram adapter if configured.
        // Publishes telegram.in:<chat_id> for incoming messages.
        // Subscribes to telegram.out:* for outgoing replies.
        if let Some(ref tg) = def.telegram {
            let token = tg.token.clone();
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            tokio::spawn(async move {
                if let Err(e) = adapters::telegram::run(token, bus, agent_name.clone()).await {
                    tracing::error!(agent = %agent_name, error = %e, "telegram adapter failed");
                }
            });
        }

        // Start schedule tasks if the user config has schedules.
        if let Some(ref ucfg) = user_cfg {
            if !ucfg.schedules.is_empty() {
                schedule::start(ucfg.schedules.clone(), bus_socket.clone(), name.clone());
                info!(agent = %name, count = ucfg.schedules.len(), "started schedules");
            }
        }

        // Start worker on the agent's bus.
        let bus = bus_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = worker::run(&name, &bus, Some(bus.clone())).await {
                tracing::error!(agent = %name, error = %e, "worker exited with error");
            }
        });
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

    let reg = serde_json::json!({"type": "register", "name": "deskd-cli-list", "subscriptions": []});
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
            if v.get("type").and_then(|t| t.as_str()) == Some("list_response") {
                if let Some(arr) = v.get("clients").and_then(|c| c.as_array()) {
                    return Ok::<_, anyhow::Error>(
                        arr.iter()
                            .filter_map(|n| n.as_str())
                            .map(|s| s.to_string())
                            .collect(),
                    );
                }
            }
        }
        Ok(Default::default())
    })
    .await;

    result.unwrap_or(Ok(Default::default()))
}
