# deskd — Agent Orchestration Runtime

Rust daemon that runs a message bus and manages Claude Code agent processes.
GitHub: github.com/kgatilin/deskd. Upstream: github.com/kira-autonoma/deskd.

## Architecture

```
deskd serve
├── bus         Unix socket pub/sub server (routes messages between clients)
├── adapters/   External integrations (Telegram, etc.) — same process, separate modules
└── workers     Per-agent loops — each connects to bus as a named client
                Each worker manages one claude CLI process (child of worker)

[Agents are separate processes connecting to deskd via Unix socket]
claude-code (agent-kira, user: agent-kira, dir: /home/agent-kira)
claude-code (agent-tmp-1, spawned ephemerally)
```

**Message flow:**
1. External event (e.g. Telegram message) → adapter → bus
2. Bus routes to agent worker's queue
3. Worker spawns `claude -p <task>` as child process, reads `stream-json` stdout
4. Worker posts result back to bus → adapter → Telegram

**Agent tools (injected via MCP/`--tool-call`):**
- `bus_send(target, payload)` — publish a message to the bus
- `agent_spawn(name, task, work_dir)` — start an ephemeral agent for a subtask

## Key Concepts

- **Bus** — Unix socket server. Clients register with a name and subscriptions. Routing: `agent:<name>`, `queue:<name>`, `broadcast`, glob patterns (`telegram:*`).
- **Adapter** — compiled-in module bridging an external system to the bus. Currently: Telegram.
- **Worker** — long-running loop for an agent: reads from bus queue, runs claude, posts result.
- **Agent state** — persisted in `~/.deskd/agents/<name>.yaml`. Includes `session_id` for claude `--resume`.
- **Ephemeral agents** — spawned on-demand by other agents, connect to bus, complete task, exit.

## Commands

```
deskd serve [--socket PATH] [--config WORKSPACE]
    Start bus server + adapters + configured agent workers.

deskd agent create <name> [--prompt TEXT] [--model MODEL] [--workdir DIR] [--max-turns N]
    Register a new agent (saves state file, does not start worker).

deskd agent run <name> [--socket PATH]
    Start the worker loop for an agent (connects to bus, processes tasks).

deskd agent send <name> <message> [--max-turns N] [--socket PATH]
    Send a task to an agent (via bus if running, direct otherwise).

deskd agent list
    Show all registered agents with their stats.

deskd agent stats <name>
    Show detailed stats for an agent (turns, cost, session_id).

deskd agent rm <name>
    Remove an agent (state + logs).
```

## Build & Run

```bash
cargo build --release
cargo test
cargo clippy -- -D warnings
cargo fmt --check

# Start bus
./target/release/deskd serve --socket /tmp/deskd.sock

# In another terminal — start an agent worker
deskd agent create kira --prompt "You are Kira." --workdir /home/agent-kira
deskd agent run kira

# Send a task
deskd agent send kira "What files are in the current directory?"
```

## Development Guidelines

### Error handling
- Use `anyhow` for application errors (executable code). For library-style modules, prefer `thiserror`.
- Never silence I/O errors with `.ok()`. At minimum log them: `if let Err(e) = ... { tracing::warn!(...) }`.
- Propagate errors with `?` and add context via `.context("what we were doing")`.

### Logging
- Use `tracing` (not `eprintln!`). Structured fields: `tracing::info!(agent = %name, "task completed")`.
- Log at appropriate levels: `error` for failures, `warn` for degraded behavior, `info` for lifecycle, `debug` for internals.

### Async
- Prefer `mpsc` channels over `Mutex<T>` for shared state across tasks.
- Use `tokio::select!` for cancellation / timeout.
- Every `tokio::spawn` task should handle its own errors — don't let them silently die.

### Testing
- Unit tests in `#[cfg(test)]` modules within the same file.
- Integration tests in `tests/` directory.
- Async tests: `#[tokio::test]`.
- Required coverage: bus routing (all patterns), message serde, worker reply flow.
- New features require tests before merge.

### Code style
- `#[derive(Debug)]` on all public types.
- No dead code in `main` branch — fix warnings before merging.
- `cargo fmt` before every commit.
- `cargo clippy -- -D warnings` must pass in CI.

## Architecture Validation

Run `archlint check .` for SOLID / coupling / cycle violations before proposing changes.
archlint MCP server is registered — it provides architecture feedback inline.

## State Files

```
~/.deskd/
├── agents/<name>.yaml    # agent config + session_id + cost tracking
└── logs/<name>.log       # claude stdout/stderr per agent
```

Config for `deskd serve`:
```yaml
# workspace.yaml
bus:
  socket: /run/deskd/bus.sock

adapters:
  telegram:
    token: "${TELEGRAM_BOT_TOKEN}"

agents:
  - name: kira
    model: claude-opus-4-6
    system_prompt: "You are Kira."
    work_dir: /home/agent-kira
    unix_user: agent-kira
    max_turns: 100
    budget_usd: 50.0
```
