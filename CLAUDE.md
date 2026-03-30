# deskd — Agent Orchestration Runtime

Lightweight runtime for managing AI agents. Spawn agents, route messages, track costs.

Repo: github.com/kgatilin/deskd (public, MIT).

## Architecture

Each agent gets its own isolated message bus (Unix socket at `{work_dir}/.deskd/bus.sock`). No shared root bus. `deskd serve` starts all agents defined in a workspace config.

```
workspace.yaml
    ↓
deskd serve
    ↓
┌─────────────────┐     ┌─────────────────┐
│ agent: uagent    │     │ agent: dev       │
│ bus.sock         │     │ bus.sock         │
│ worker (Claude)  │     │ worker (Claude)  │
│ [telegram]       │     │                  │
└─────────────────┘     └─────────────────┘
```

## Source Layout

```
src/
  main.rs       — CLI (clap): serve, agent {create,send,run,list,stats,read,rm,spawn}, mcp, graph {run,validate}
  config.rs     — WorkspaceConfig (workspace.yaml) + UserConfig (deskd.yaml) parsing
  agent.rs      — Agent state, create/recover, send() and send_streaming() to Claude
  bus.rs        — Per-agent Unix socket message bus (pub/sub with glob routing)
  worker.rs     — Worker loop: read from bus → execute via Claude → post result + write inbox
  inbox.rs      — File-based inbox: write task results to ~/.deskd/inbox/<sender>/
  mcp.rs        — MCP server (stdio, newline-delimited JSON): send_message, add_persistent_agent, run_graph
  graph.rs      — Executable skill graph engine: DAG of tool groups + LLM decision nodes
  message.rs    — Message/Envelope types for bus protocol
  schedule.rs   — Cron-based scheduled actions on the bus
  adapters/
    telegram.rs — Telegram bot adapter (teloxide): polls messages, routes to/from bus
```

## Config Files

### workspace.yaml — defines agents for `deskd serve`
```yaml
agents:
  - name: dev
    work_dir: /home/dev
    command: [claude, --output-format, stream-json, ...]
    telegram:                    # optional
      token: ${BOT_TOKEN}
    budget_usd: 50.0             # optional, default 50
```

### deskd.yaml — per-agent config (at `{work_dir}/deskd.yaml`)
```yaml
model: claude-sonnet-4-6
system_prompt: "You are..."
max_turns: 100
mcp_config: '{"mcpServers":{...}}'
telegram:
  routes:
    - chat_id: -1234567890
agents: []         # sub-agents
schedules: []      # cron jobs
channels: []       # named bus targets
```

## Key Paths

| Path | Purpose |
|------|---------|
| `{work_dir}/.deskd/bus.sock` | Agent's message bus socket |
| `~/.deskd/agents/{name}.yaml` | Agent state (cost, turns, session_id) |
| `~/.deskd/inbox/{sender}/` | Task results for async reading |
| `~/.deskd/logs/{name}.log` | Agent logs |

## CLI

```bash
# Start all agents
deskd serve --config workspace.yaml

# Send task to agent
deskd agent send <name> "task" --socket <bus.sock>

# Read replies
deskd agent read <sender>          # e.g. "cli"
deskd agent read <sender> --clear  # read and delete

# Agent management
deskd agent list --socket <bus.sock>
deskd agent stats <name>
deskd agent create <name> [--prompt ...] [--model ...]
deskd agent rm <name>

# Skill graphs
deskd graph run <file.yaml>              # execute a graph
deskd graph run <file.yaml> --work-dir . # custom work dir
deskd graph validate <file.yaml>         # validate without running

# MCP server (called by Claude via --mcp-config)
deskd mcp --agent <name>
```

## Bus Protocol

Unix socket, newline-delimited JSON.

Register: `{"type":"register","name":"cli","subscriptions":["agent:cli"]}`
Message: `{"type":"message","id":"uuid","source":"cli","target":"agent:dev","payload":{"task":"..."}}`
List: `{"type":"list"}` → `{"type":"list_response","clients":[...]}`

Routing targets:
- `agent:<name>` — direct to agent
- `queue:<name>` — subscription-based multicast
- `telegram.out:<chat_id>` — to Telegram adapter
- `broadcast` — to all clients
- Glob patterns: `telegram.in:*`, `agent:*`

## MCP Server

Newline-delimited JSON-RPC on stdio (NOT Content-Length framed). Tools:
- `send_message(target, text)` — publish to bus
- `add_persistent_agent(name, model, system_prompt, subscribe)` — spawn sub-agent
- `run_graph(file, work_dir)` — execute a skill graph YAML file

Env vars injected by deskd into Claude process:
- `DESKD_BUS_SOCKET` — bus socket path
- `DESKD_AGENT_NAME` — agent name
- `DESKD_AGENT_CONFIG` — path to deskd.yaml

## Build

```bash
# Development
cargo build
cargo test

# Release (native)
cargo build --release

# Release (Linux static, for containers/VPS)
cargo build --release --target x86_64-unknown-linux-musl

# macOS: re-sign after build
codesign --force --sign - target/release/deskd
```

## Install

```bash
# From GitHub Releases (prebuilt binary)
curl -fsSL https://raw.githubusercontent.com/kgatilin/deskd/main/install.sh | bash
```

## Before Pushing

All three must pass — CI will reject otherwise:

```bash
cargo fmt --check    # formatting (auto-fix: cargo fmt)
cargo clippy -- -D warnings   # linter/static analysis
cargo test           # unit tests
```

Quick one-liner:
```bash
cargo fmt && cargo clippy -- -D warnings && cargo test
```

## CI/CD

- **CI** (push/PR to main): `cargo fmt --check` + `cargo clippy -D warnings` + `cargo test`
- **Release** (tag `v*`): quality gate → build binaries (Linux amd64, macOS amd64/arm64) → GitHub Release

Releases are built by GitHub Actions on tag push. Download from GitHub Releases.

## Formatting

Rust files are auto-formatted via two mechanisms:
- **Claude Code hook**: `.claude/settings.json` runs `rustfmt` on every .rs file after Write/Edit
- **Git pre-commit hook**: `.githooks/pre-commit` runs `cargo fmt --check` before commit

If you edit .rs files, formatting is handled automatically. No need to run `cargo fmt` manually.

## QA Process

### Issues: Acceptance Criteria
Every issue/ticket MUST include an **Acceptance Criteria** section before work begins:
```
## Acceptance Criteria
- [ ] Specific, testable condition 1
- [ ] Specific, testable condition 2
- [ ] Tests pass: cargo test
- [ ] Quality gate: cargo fmt + clippy + test
```
If an issue lacks acceptance criteria, add them before starting work.

### Issue Triage: `agent-ready` Label
The `agent-ready` label means "this issue is ready for an agent to pick up and implement." Do NOT add `agent-ready` until the issue has acceptance criteria. Flow:
1. Issue created → check for acceptance criteria
2. No criteria → add them first, THEN label `agent-ready`
3. Has criteria → label `agent-ready`

### PR Review: Checklist
Every PR review MUST verify each acceptance criterion from the linked issue:
1. Read the linked issue's acceptance criteria
2. Check each criterion against the diff — does the code satisfy it?
3. Run quality gate: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
4. If any criterion is not met, request changes with specific feedback
5. Only merge when ALL criteria are checked off

## Conventions

- Merge directly to main (squash merge, no PRs for owner repos)
- Commit messages: `description (#issue)` or `description`
- All quality gates must pass before merge
- RUST_LOG=debug for verbose logging to stderr
