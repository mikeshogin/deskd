mod agent;
mod bus;
mod config;
mod message;
mod worker;

use clap::{Parser, Subcommand};

const DEFAULT_SOCKET: &str = "/tmp/deskd.sock";

#[derive(Parser)]
#[command(name = "deskd", about = "Agent orchestration runtime")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create and start a new agent
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Start the message bus server
    Serve {
        /// Unix socket path
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Create a new agent
    Create {
        /// Agent name
        name: String,
        /// System prompt
        #[arg(long)]
        prompt: Option<String>,
        /// Model to use
        #[arg(long, default_value = "claude-sonnet-4-20250514")]
        model: String,
        /// Working directory
        #[arg(long)]
        workdir: Option<String>,
        /// Max turns per task
        #[arg(long, default_value = "100")]
        max_turns: u32,
    },
    /// Send a message to an agent
    Send {
        /// Agent name
        name: String,
        /// Message to send
        message: String,
        /// Max turns for this task
        #[arg(long)]
        max_turns: Option<u32>,
        /// Bus socket path
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Run agent worker loop (connects to bus, processes tasks)
    Run {
        /// Agent name
        name: String,
        /// Bus socket path
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// List all agents
    List,
    /// Show agent stats (turns, cost)
    Stats {
        /// Agent name
        name: String,
    },
    /// Remove an agent
    Rm {
        /// Agent name
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Agent { action } => match action {
            AgentAction::Create {
                name,
                prompt,
                model,
                workdir,
                max_turns,
            } => {
                let cfg = agent::AgentConfig {
                    name: name.clone(),
                    model,
                    system_prompt: prompt.unwrap_or_default(),
                    work_dir: workdir.unwrap_or_else(|| ".".into()),
                    max_turns,
                };
                let state = agent::create(&cfg).await?;
                println!("Agent {} created (pid: {})", name, state.pid);
            }
            AgentAction::Send {
                name,
                message,
                max_turns,
                socket,
            } => {
                // If bus socket exists, route through bus; otherwise direct
                if std::path::Path::new(&socket).exists() {
                    // If name contains ':', treat as raw target (e.g. telegram:-123)
                    let target = if name.contains(':') { name.clone() } else { format!("agent:{}", name) };
                    worker::send_via_bus(&socket, "cli", &target, &message, max_turns).await?;
                } else {
                    let response = agent::send(&name, &message, max_turns).await?;
                    println!("{}", response);
                }
            }
            AgentAction::Run { name, socket } => {
                // Verify agent exists
                agent::load_state(&name)?;

                eprintln!("[deskd] starting worker for agent '{}'", name);
                tokio::select! {
                    result = worker::run(&name, &socket) => {
                        result?;
                    }
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("\n[deskd] shutting down agent '{}'", name);
                    }
                }
            }
            AgentAction::List => {
                let agents = agent::list().await?;
                if agents.is_empty() {
                    println!("No agents running");
                } else {
                    println!("{:<15} {:<10} {:<8} {:<10} {}", "NAME", "PID", "TURNS", "COST", "MODEL");
                    for a in agents {
                        println!(
                            "{:<15} {:<10} {:<8} ${:<9.2} {}",
                            a.config.name, a.pid, a.total_turns, a.total_cost, a.config.model
                        );
                    }
                }
            }
            AgentAction::Stats { name } => {
                let state = agent::load_state(&name)?;
                println!("Agent: {}", state.config.name);
                println!("Model: {}", state.config.model);
                println!("PID: {}", state.pid);
                println!("Total turns: {}", state.total_turns);
                println!("Total cost: ${:.4}", state.total_cost);
                println!("Created: {}", state.created_at);
            }
            AgentAction::Rm { name } => {
                agent::remove(&name).await?;
                println!("Agent {} removed", name);
            }
        },
        Commands::Serve { socket } => {
            bus::serve(&socket).await?;
        }
    }

    Ok(())
}
