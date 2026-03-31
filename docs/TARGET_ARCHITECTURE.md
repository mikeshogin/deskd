# deskd — Target Architecture

## Current State

16,600 строк Rust. Монолитная структура: 20 модулей в плоском `src/`, 1 trait на всю кодобазу (`Adapter`), 35% кода без тестов. Всё зависит от всего через конкретные типы.

```
src/
  main.rs          2,011 loc  ← god module, 0 tests
  agent.rs         1,506 loc  ← domain + infra mixed, 0 tests
  mcp.rs           1,297 loc  ← monolithic handler, 0 tests
  graph.rs         1,384 loc  ← OK, 16 tests
  schedule.rs      1,252 loc  ← mixed concerns
  adapters/
    telegram.rs    1,248 loc
    discord.rs       290 loc
    mod.rs            52 loc  ← единственный trait
  worker.rs          991 loc  ← 400-line god function, 0 tests
  acp.rs             951 loc  ← duplicates agent patterns, 17 tests
  config.rs          866 loc  ← god config, fan-in 12
  workflow.rs        680 loc  ← OK
  tasklog.rs         604 loc
  task.rs            521 loc  ← clean, 11 tests
  bus.rs             491 loc  ← clean, 14 tests
  unified_inbox.rs   475 loc  ← clean, 9 tests
  context.rs         460 loc  ← clean, 12 tests
  statemachine.rs    448 loc  ← clean, 10 tests
  message.rs         126 loc  ← clean, 7 tests
  inbox.rs           123 loc  ← legacy, duplicates unified_inbox
```

---

## Target Architecture

### Domain Model

Три bounded contexts:

```
┌─────────────────────────────────────────────────┐
│                   deskd                          │
│                                                  │
│  ┌──────────┐  ┌──────────┐  ┌───────────────┐  │
│  │ Messaging│  │  Agents  │  │  Orchestration│  │
│  │          │  │          │  │               │  │
│  │ Message  │  │ Agent    │  │ Task          │  │
│  │ Envelope │  │ State    │  │ StateMachine  │  │
│  │ Inbox    │  │ Config   │  │ Workflow      │  │
│  │ Bus      │  │ Runner   │  │ Graph         │  │
│  │          │  │          │  │ Schedule      │  │
│  └──────────┘  └──────────┘  └───────────────┘  │
│                                                  │
│  ┌──────────────────────────────────────────┐    │
│  │              Adapters (ports)            │    │
│  │  Telegram │ Discord │ MCP │ ACP │ CLI   │    │
│  └──────────────────────────────────────────┘    │
│                                                  │
│  ┌──────────────────────────────────────────┐    │
│  │           Infrastructure                 │    │
│  │  UnixBus │ FileStore │ Process │ Paths  │    │
│  └──────────────────────────────────────────┘    │
└─────────────────────────────────────────────────┘
```

### Module Layout

```
src/
  lib.rs

  # ── Domain (pure types, traits, no I/O) ──────────
  domain/
    mod.rs
    message.rs        # Message, Envelope, Register, Metadata
    agent.rs          # AgentConfig, AgentState, TurnResult, TokenUsage, TaskLimits
    task.rs           # Task, TaskStatus, TaskCriteria, QueueSummary
    statemachine.rs   # Instance, Transition
    inbox.rs          # InboxMessage
    config.rs         # UserConfig, AgentDef, SubAgentDef, ModelDef, etc.
    context.rs        # ContextConfig, Node, MainBranch

  # ── Ports (trait definitions) ─────────────────────
  ports/
    mod.rs
    runner.rs         # trait AgentRunner { send, send_streaming, is_alive, restart }
    bus.rs            # trait MessageBus { send, subscribe, register }
    stores.rs         # trait TaskStore, StateMachineStore, InboxStore, TaskLogStore

  # ── Infrastructure (trait impls, I/O) ─────────────
  infra/
    mod.rs
    unix_bus.rs       # impl MessageBus for UnixBus
    file_stores.rs    # impl TaskStore for FileTaskStore, etc.
    claude_runner.rs  # impl AgentRunner — Claude CLI subprocess
    acp_runner.rs     # impl AgentRunner — ACP JSON-RPC
    paths.rs          # state_dir(), log_dir(), reminders_dir()
    process.rs        # subprocess spawning, stream-json parsing

  # ── Adapters (external platform bridges) ──────────
  adapters/
    mod.rs            # trait Adapter (already exists)
    telegram.rs
    discord.rs

  # ── Application (orchestration, use cases) ────────
  app/
    mod.rs
    worker.rs         # thin orchestrator: MessageHandler + BudgetChecker + QueuePoller + ...
    serve.rs          # serve() — wires bus + agents + adapters + schedule + workflow
    schedule.rs       # cron scheduling (without GitHub polling infra)
    workflow.rs       # workflow engine
    graph.rs          # skill graph execution
    mcp.rs            # MCP server (tool listing + dispatch)

  # ── CLI ───────────────────────────────────────────
  cli/
    mod.rs            # clap structs
    commands.rs       # task, schedule, status, agent command handlers

  main.rs             # thin entry: parse CLI → dispatch
```

### Key Traits (Ports)

```rust
// ── ports/runner.rs ──

/// Abstraction over agent execution (Claude CLI, ACP, future runtimes).
pub trait AgentRunner: Send {
    async fn send(
        &mut self,
        task: &str,
        state: &mut AgentState,
        limits: &TaskLimits,
    ) -> Result<TurnResult>;

    async fn send_streaming(
        &mut self,
        task: &str,
        state: &mut AgentState,
        limits: &TaskLimits,
        on_chunk: Box<dyn Fn(&str) + Send>,
    ) -> Result<TurnResult>;

    fn is_alive(&self) -> bool;
    async fn restart(&mut self) -> Result<()>;
}
```

```rust
// ── ports/bus.rs ──

/// Abstraction over message transport.
pub trait MessageBus: Send + Sync {
    async fn send(&self, msg: &Message) -> Result<()>;
    async fn recv(&self) -> Result<Message>;
    async fn register(&self, name: &str, subscriptions: &[String]) -> Result<()>;
}
```

```rust
// ── ports/stores.rs ──

/// Task queue persistence.
pub trait TaskRepository: Send + Sync {
    fn create(&self, description: &str, criteria: TaskCriteria, created_by: &str) -> Result<Task>;
    fn load(&self, id: &str) -> Result<Task>;
    fn list(&self, status: Option<TaskStatus>) -> Result<Vec<Task>>;
    fn claim_next(&self, agent: &str, model: &str, labels: &[String]) -> Result<Option<Task>>;
    fn complete(&self, id: &str, result: &str) -> Result<Task>;
    fn fail(&self, id: &str, error: &str) -> Result<Task>;
    fn cancel(&self, id: &str) -> Result<Task>;
}

/// State machine persistence.
pub trait StateMachineRepository: Send + Sync {
    fn save(&self, inst: &Instance) -> Result<()>;
    fn load(&self, id: &str) -> Result<Instance>;
    fn list(&self) -> Result<Vec<Instance>>;
}

/// Unified inbox persistence.
pub trait InboxRepository: Send + Sync {
    fn write(&self, inbox: &str, msg: &InboxMessage) -> Result<()>;
    fn read(&self, inbox: &str, limit: usize, offset: usize) -> Result<Vec<InboxMessage>>;
    fn search(&self, inbox: &str, query: &str, limit: usize) -> Result<Vec<InboxMessage>>;
    fn list_inboxes(&self) -> Result<Vec<String>>;
}
```

### Dependency Rules

```
                    ┌──────────┐
                    │  domain   │  ← depends on NOTHING
                    └────┬─────┘
                         │
                    ┌────▼─────┐
                    │  ports    │  ← depends on domain only
                    └────┬─────┘
                         │
            ┌────────────┼────────────┐
            │            │            │
       ┌────▼─────┐ ┌───▼────┐ ┌────▼─────┐
       │  infra    │ │  app   │ │ adapters │
       └──────────┘ └────────┘ └──────────┘
            │            │            │
            └────────────┼────────────┘
                         │
                    ┌────▼─────┐
                    │   cli    │
                    └────┬─────┘
                         │
                    ┌────▼─────┐
                    │  main.rs │  ← wires everything together
                    └──────────┘
```

**Правила:**
- `domain` не импортирует ничего из deskd (только std + serde)
- `ports` импортирует только `domain`
- `infra` импортирует `domain` + `ports` (реализует traits)
- `app` импортирует `domain` + `ports` (работает через traits, не знает про infra)
- `adapters` импортирует `domain` + `ports`
- `cli` импортирует `domain` + `app`
- `main.rs` — единственное место где infra подключается к app (dependency injection)

### Worker: Before → After

**Before (god function):**
```rust
pub async fn run(name: &str, bus_socket: &str, ...) {
    // 400 lines: bus connect, register, parse messages,
    // check budget, send typing indicators, stream progress,
    // poll task queue, handle crashes, write inbox, write tasklog,
    // update SM, reply to bus...
}
```

**After (thin orchestrator):**
```rust
pub struct Worker {
    runner: Box<dyn AgentRunner>,
    bus: Arc<dyn MessageBus>,
    tasks: Arc<dyn TaskRepository>,
    inbox: Arc<dyn InboxRepository>,
    tasklog: Arc<dyn TaskLogRepository>,
}

impl Worker {
    pub async fn run(&mut self) {
        loop {
            tokio::select! {
                msg = self.bus.recv() => self.handle_message(msg).await,
                task = self.poll_queue() => self.handle_task(task).await,
            }
        }
    }

    async fn handle_message(&mut self, msg: Message) {
        let budget = BudgetChecker::check(&self.state);
        if budget.exceeded { return self.reject(msg).await; }

        let result = self.runner.send_streaming(
            &msg.task(), &mut self.state, &self.limits,
            self.progress_reporter(msg.reply_to()),
        ).await;

        match result {
            Ok(turn) => self.complete(msg, turn).await,
            Err(e) => self.fail(msg, e).await,
        }
    }
}
```

### Config: Before → After

**Before:** 16 struct'ов, 866 строк, fan-in 12, path helpers + MCP description generation.

**After:**
- `domain/config.rs` — UserConfig, AgentDef, SubAgentDef, ModelDef (pure data)
- `infra/paths.rs` — state_dir(), log_dir(), reminders_dir()
- `app/mcp.rs` — send_message_description()
- TaskCriteria stays in domain/task.rs, TransitionDef references it via domain, not cross-module

### archlint Validation Gates

Каждый этап рефакторинга валидируется archlint-rs:

| Metric | Current | Target | Gate |
|--------|---------|--------|------|
| Health score | 35/100 | ≥75/100 | CI блокирует merge при снижении |
| Fan-out max | 18 (main) | ≤7 | `rules.fan_out.threshold: 7` |
| Fan-in max | 12 (config) | ≤10 | `rules.fan_in.threshold: 10` |
| DIP violations | 9 | 0 | Каждый модуль с public API должен иметь trait |
| Cyclomatic complexity | 72 (main) | ≤15 | `perf` check в CI |
| Untested modules | 4 (35% LOC) | 0 | coverage gate |

### Roadmap

```
Phase 1: Foundations (traits)            ← Issues #138, #139, #140
  - AgentRunner trait
  - MessageBus trait
  - Store traits
  archlint gate: DIP violations → 0

Phase 2: Decomposition                  ← Issues #141, #142, #143, #144
  - Break up worker::run()
  - Consolidate inboxes
  - Break up main.rs
  - Fix config module
  archlint gate: fan-out ≤7, fan-in ≤10

Phase 3: Testing                        ← Issue #145
  - Tests for agent, worker, mcp
  - Integration tests with InMemoryBus + InMemoryStore
  archlint gate: 0 untested modules

Phase 4: CI enforcement
  - archlint-rs quality gate in CI
  - Block merge on health score regression
  - Block merge on new DIP/fan-out violations
```

---

## Принципы

1. **Каждый модуль — API.** Public types и functions = контракт. Стабильный, лаконичный, расширяемый без breaking changes.

2. **Domain не знает про infrastructure.** AgentState не знает что хранится в YAML файле. Task не знает что лежит в `~/.deskd/tasks/`. Это знает только infra.

3. **Зависимости текут внутрь.** Adapters → Ports → Domain. Никогда наоборот. main.rs — единственное место где внешнее подключается к внутреннему.

4. **Traits на границах.** Везде где модуль A использует модуль B — через trait. Конкретные типы видит только main.rs при wiring.

5. **Open/Closed.** Новый adapter (Slack) = новый файл в adapters/. Новый runner (Ollama) = новый файл в infra/. Существующий код не меняется.

6. **Тесты без I/O.** InMemoryBus, InMemoryStore, MockRunner. Unit tests не трогают файловую систему и сокеты.
