# deskd Roadmap

## Philosophy

deskd is a lightweight agent runtime. Keep it simple:
- **Task queue**: add task → agent picks it up → result in inbox
- **Budget control**: deskd manages spend per agent, not the agent itself
- **Workflows stay on the agent side**: agents use skills/instructions to manage their own workflow stages
- **deskd doesn't know about code**: no git, no worktrees, no Jira — that's the agent's domain

## Current State (v0.1)

- Per-agent isolated bus (socket-based pub/sub)
- Worker executes tasks via Claude CLI
- File-based inbox for async replies (`deskd agent read cli`)
- Telegram adapter (streaming)
- Agent stats (cost, turns, session)
- MCP server (send_message, add_persistent_agent)
- Workspace config for multi-agent setup

## Iteration 1 — Task Queue & Visibility

**Goal**: manage tasks as first-class objects, not fire-and-forget bus messages.

- [ ] **Persistent task queue** — tasks survive agent/bus restarts. Store in `~/.deskd/tasks/<agent>/` as files (pending/active/done).
- [ ] `deskd task add <agent> "description"` — queue a task with optional `--priority`
- [ ] `deskd task list <agent>` — show pending, active, completed tasks
- [ ] `deskd task cancel <agent> <id>` — remove from queue before pickup
- [ ] Worker pulls from task queue instead of raw bus messages
- [ ] **Status dashboard** (`deskd status --config <workspace.yaml>`) — all agents: idle/working/offline, current task, queue depth (#28)
- [ ] **Stale session handling** — retry without `--resume` on failure (#25)

Related issues: #25, #28, #30

## Iteration 2 — Streaming & Progress

**Goal**: see what agents are doing in real-time, same model for all senders.

- [ ] **Unified streaming** — worker always uses `send_streaming()`, streams to `reply_to` target (#29)
- [ ] **Inbox writer as bus subscriber** — writes progress entries to inbox as they arrive
- [ ] `deskd agent read cli --follow` — tail inbox, show live updates
- [ ] **Task status updates** — pending → active → streaming → done/failed
- [ ] Progress visible in `deskd status` dashboard

Related issues: #29

## Iteration 3 — Budget & Scheduling

**Goal**: deskd controls spend, not the agent. Different strategies per agent.

- [ ] **Budget tracking per agent** — already have `total_cost` and `budget_usd` in agent state
- [ ] **Time-windowed budgets** — e.g. $5/hour, $50/day. deskd pauses the agent when budget exhausted for the window, resumes next window
- [ ] **Scheduling strategies**:
  - `immediate` — process tasks as they come (current behavior)
  - `spread` — distribute tasks evenly across the budget window (e.g. 10 tasks, $5 budget → aim for $0.50 each, pace over the day)
  - `burst` — process queue ASAP until budget runs out, then wait
- [ ] **Task cost estimation** — before executing, estimate cost based on task complexity (simple heuristic: prompt length, model choice). deskd decides whether to proceed or defer.
- [ ] **Per-task budget cap** — `deskd task add <agent> "task" --max-cost 2.00`
- [ ] Config in `deskd.yaml` per agent:
  ```yaml
  budget:
    daily_usd: 20.00
    strategy: spread
    max_task_usd: 5.00
  ```

## Iteration 4 — Container Isolation

**Goal**: agents run in Docker containers, not bare processes.

- [ ] **Container backend in workspace.yaml** — `runtime: docker` per agent, with image, mounts, env
- [ ] Leverage myhome container profiles (`claude-personal`, `claude-uagent`) — deskd reads container config from myhome.yml or its own config
- [ ] Agent process = `docker run` instead of bare `claude` command
- [ ] Remove `--dangerously-skip-permissions` — container is the sandbox
- [ ] **Ephemeral sub-agents in containers** — `add_persistent_agent` spawns a container
- [ ] Git backup on container exit (myhome already does this)

Related: myhome#80, myhome#81, myhome#74

## Iteration 5 — Multi-Agent Coordination

**Goal**: agents can delegate to each other, admin session manages the fleet.

- [ ] **Inbox access control** (#27)
- [ ] **Agent-to-agent task delegation** — agent:dev can `deskd task add uagent "review my PR"`
- [ ] **Admin CLI** — unified view across all agents from the interactive session
- [ ] **Priority scheduling across agents** — global budget allocation, not just per-agent

## Out of Scope (stays in myhome)

- Repo sync, worktree management
- Container image builds
- VPS deployment (`myhome sync`)
- Git identity management
- Package/tool installation

## Out of Scope (stays on agent side)

- Workflow stages (skills, instructions in CLAUDE.md)
- Jira/Confluence integration
- Code review process
- Merge strategy
