# deskd — Target Architecture

## Vision

deskd — runtime для управления контекстами AI-агентов. Центральная сущность — **Context**, не Agent. Agent (Claude, ACP, Gemini) — это просто executor, инфраструктура. Ценность в курируемом контексте, который делает каждого агента эффективным.

**Context** — граф из нод, определяющий что агент знает и может: какие документы прочитать, какую память загрузить, какие тулы вызвать, какой статус проверить. Всё это материализуется ДО первого сообщения пользователя.

"Переключиться на PM-агента" = загрузить PM-контекст в executor.

---

## Current State

16,600 строк Rust. Монолитная структура: 20 модулей в плоском `src/`, 1 trait на всю кодобазу (`Adapter`), 35% кода без тестов. Всё зависит от всего через конкретные типы.

```
src/
  main.rs          2,011 loc  ← god module, 0 tests
  agent.rs         1,506 loc  ← domain + infra mixed, 0 tests
  mcp.rs           1,297 loc  ← monolithic handler, 0 tests
  graph.rs         1,384 loc  ← skill graph engine, 16 tests
  schedule.rs      1,252 loc  ← mixed concerns
  adapters/
    telegram.rs    1,248 loc
    discord.rs       290 loc
    mod.rs            52 loc  ← единственный trait
  worker.rs          991 loc  ← 400-line god function, 0 tests
  acp.rs             951 loc  ← duplicates agent patterns, 17 tests
  config.rs          866 loc  ← god config, fan-in 12
  workflow.rs        680 loc
  tasklog.rs         604 loc
  task.rs            521 loc  ← clean, 11 tests
  bus.rs             491 loc  ← clean, 14 tests
  unified_inbox.rs   475 loc  ← clean, 9 tests
  context.rs         460 loc  ← context graph, 12 tests
  statemachine.rs    448 loc  ← clean, 10 tests
  message.rs         126 loc  ← clean, 7 tests
  inbox.rs           123 loc  ← legacy, duplicates unified_inbox
```

---

## Domain Model

### Core Entities

```
Context
  │
  ├── ContextGraph          ← граф из нод, определяющий что агент знает
  │     ├── ReadDocs        ← прочитать файлы/документы
  │     ├── LoadMemory      ← загрузить релевантную память
  │     ├── CallTool        ← вызвать тул (git status, check CI, etc.)
  │     ├── Prompt          ← system prompt + instructions
  │     └── Branch          ← условное ветвление
  │
  ├── Session               ← материализованный контекст (результат выполнения графа)
  │     ├── documents[]     ← прочитанные документы
  │     ├── memory[]        ← загруженная память
  │     ├── tool_results[]  ← результаты тулов
  │     └── system_prompt   ← финальный system prompt
  │
  └── ExecutionGraph        ← граф повторяемых операций (текущий graph.rs)
        ├── Step            ← нода (tool call, LLM decision, bash)
        ├── Condition       ← ветвление по результату
        └── Compression     ← сжатие контекста между шагами

Message                     ← роутинг: Envelope, target, payload
Task                        ← pull-based очередь асинхронных задач
StateMachine                ← lifecycle management (states, transitions)
```

### Bounded Contexts

```
┌─────────────────────────────────────────────────────────┐
│                        deskd                             │
│                                                          │
│  ┌────────────────┐  ┌──────────┐  ┌─────────────────┐  │
│  │    Context      │  │ Messaging│  │  Orchestration  │  │
│  │    (core)       │  │          │  │                 │  │
│  │                 │  │ Message  │  │ Task            │  │
│  │ ContextGraph    │  │ Envelope │  │ StateMachine    │  │
│  │ Session         │  │ Inbox    │  │ Workflow        │  │
│  │ ExecutionGraph  │  │          │  │ Schedule        │  │
│  │                 │  │          │  │                 │  │
│  └────────────────┘  └──────────┘  └─────────────────┘  │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │              Adapters (ports)                     │    │
│  │  Telegram │ Discord │ MCP │ CLI                  │    │
│  └──────────────────────────────────────────────────┘    │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │              Infrastructure                       │    │
│  │  Executors (Claude/ACP) │ UnixBus │ FileStore    │    │
│  └──────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

**Context — core bounded context.** Вокруг него строится всё остальное. Messaging и Orchestration — supporting contexts.

---

## Target Module Layout

```
src/
  lib.rs

  # ── Domain (pure types + traits, no I/O) ─────────
  domain/
    mod.rs
    context.rs        # ContextGraph, ContextNode, Session — CORE
    graph.rs          # ExecutionGraph, Step, StepType, Condition
    message.rs        # Message, Envelope, Register, Metadata
    task.rs           # Task, TaskStatus, TaskCriteria
    statemachine.rs   # Instance, Transition
    inbox.rs          # InboxMessage
    config.rs         # UserConfig, AgentDef, ModelDef (pure data)

  # ── Ports (trait definitions) ─────────────────────
  ports/
    mod.rs
    executor.rs       # trait Executor { execute(session) → result }
    bus.rs            # trait MessageBus { send, recv, register }
    stores.rs         # trait ContextStore, TaskStore, SMStore, InboxStore

  # ── Infrastructure (trait impls, I/O) ─────────────
  infra/
    mod.rs
    claude.rs         # impl Executor — Claude CLI subprocess
    acp.rs            # impl Executor — ACP JSON-RPC
    unix_bus.rs       # impl MessageBus — Unix socket
    file_stores.rs    # impl *Store — JSON/YAML file persistence
    paths.rs          # state_dir(), log_dir(), etc.

  # ── Adapters (external platform bridges) ──────────
  adapters/
    mod.rs            # trait Adapter { name, run }
    telegram.rs
    discord.rs

  # ── Application (orchestration, use cases) ────────
  app/
    mod.rs
    worker.rs         # thin loop: recv message → load context → execute → reply
    serve.rs          # wires bus + agents + adapters + schedule
    schedule.rs       # cron scheduling
    workflow.rs       # SM-driven workflow engine
    mcp.rs            # MCP server (tool listing + dispatch)

  # ── CLI ───────────────────────────────────────────
  cli/
    mod.rs            # clap structs
    commands.rs       # subcommand handlers

  main.rs             # thin entry: parse CLI → wire infra → dispatch
```

---

## Key Traits (Ports)

### Executor (бывший AgentRunner)

```rust
/// Abstraction over LLM execution backends.
/// Claude CLI, ACP, Gemini, Ollama — all implement this.
/// The executor is stateless infrastructure — it receives a Session
/// and returns a result. It does not own context or state.
pub trait Executor: Send {
    async fn execute(
        &mut self,
        session: &Session,
        limits: &TaskLimits,
    ) -> Result<TurnResult>;

    async fn execute_streaming(
        &mut self,
        session: &Session,
        limits: &TaskLimits,
        on_chunk: Box<dyn Fn(&str) + Send>,
    ) -> Result<TurnResult>;

    fn is_alive(&self) -> bool;
    async fn restart(&mut self) -> Result<()>;
}
```

### MessageBus

```rust
/// Abstraction over message transport.
pub trait MessageBus: Send + Sync {
    async fn send(&self, msg: &Message) -> Result<()>;
    async fn recv(&self) -> Result<Message>;
    async fn register(&self, name: &str, subscriptions: &[String]) -> Result<()>;
}
```

### Stores

```rust
/// Context persistence.
pub trait ContextStore: Send + Sync {
    fn save(&self, name: &str, graph: &ContextGraph) -> Result<()>;
    fn load(&self, name: &str) -> Result<ContextGraph>;
    fn list(&self) -> Result<Vec<String>>;
}

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

---

## Dependency Rules

```
                    ┌──────────┐
                    │  domain   │  ← depends on NOTHING (std + serde only)
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
                    │  main.rs │  ← wires everything (DI)
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

---

## Context Flow: How It Works

### Переключение контекста

```
User: "Переключись на PM-агента"

1. Load ContextGraph("pm") from ContextStore
2. Materialize:
   ├── ReadDocs("roadmap.md", "backlog.md")
   ├── CallTool("gh issue list -R kgatilin/deskd --state open")
   ├── LoadMemory("pm-decisions")
   └── Prompt("You are a product manager for deskd...")
3. Build Session { documents, tool_results, memory, system_prompt }
4. Hand Session to Executor (Claude/ACP)
5. User talks to "PM agent" with full context already loaded
```

### Mapping to existing code

| Current | Becomes | Notes |
|---------|---------|-------|
| `context.rs` ContextConfig, Node, MainBranch | `domain/context.rs` ContextGraph, ContextNode | Already has graph structure + materialize() |
| `graph.rs` GraphDef, StepDef | `domain/graph.rs` ExecutionGraph | Already has DAG execution with conditions |
| `agent.rs` AgentProcess | `infra/claude.rs` ClaudeExecutor | Process management = infrastructure |
| `acp.rs` AcpProcess | `infra/acp.rs` AcpExecutor | JSON-RPC = infrastructure |
| `worker.rs` run() | `app/worker.rs` Worker struct | Thin orchestrator with injected deps |
| `bus.rs` serve() | `infra/unix_bus.rs` UnixBus | Transport = infrastructure |
| `task.rs` TaskStore | `infra/file_stores.rs` FileTaskStore | Persistence = infrastructure |
| `config.rs` UserConfig | `domain/config.rs` (pure data) + `infra/paths.rs` | Split pure types from I/O helpers |
| `inbox.rs` + `unified_inbox.rs` | `infra/file_stores.rs` FileInboxStore | Consolidate into one impl |

---

## Worker: Before → After

**Before (god function, 400 lines):**
```rust
pub async fn run(name: &str, bus_socket: &str, ...) {
    // bus connect, register, parse messages, check budget,
    // Telegram typing, stream progress, poll task queue,
    // crash recovery, write inbox, write tasklog,
    // update SM, reply to bus...
}
```

**After (thin orchestrator):**
```rust
pub struct Worker {
    executor: Box<dyn Executor>,
    bus: Arc<dyn MessageBus>,
    contexts: Arc<dyn ContextStore>,
    tasks: Arc<dyn TaskRepository>,
    inbox: Arc<dyn InboxRepository>,
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
        // 1. Load context for this agent
        let graph = self.contexts.load(&self.name)?;
        // 2. Materialize into session
        let session = graph.materialize().await?;
        // 3. Add the incoming task to session
        let session = session.with_task(&msg.task());
        // 4. Execute
        let result = self.executor.execute_streaming(
            &session, &self.limits, self.progress_reporter(),
        ).await;
        // 5. Handle result
        match result {
            Ok(turn) => self.complete(msg, turn).await,
            Err(e) => self.fail(msg, e).await,
        }
    }
}
```

---

## archlint Integration

### archlint-rs must validate

| Rule | What it checks | Current → Target |
|------|---------------|-----------------|
| `fan_out` | Module coupling | main: 18→≤7, worker: 7→≤5 |
| `fan_in` | Instability | config: 12→≤8 |
| `dip` | Traits exist for public modules | 9 violations → 0 |
| `layers` | Dependency direction | ❌ not supported → must add |
| `forbidden_deps` | domain→infra prohibited | ❌ not supported → must add |
| `perf` CC | Function complexity | main: CC=72→≤15 |

### What archlint-rs needs (issues for Misha)

| Issue | What | Why |
|-------|------|-----|
| mshogin/archlint#92 | Config schema compatibility | deskd config silently ignored |
| mshogin/archlint#93 | Layer violation detection | Can't enforce domain→infra rule |
| mshogin/archlint#94 | Perf false positives | 875 issues, mostly noise |
| **NEW** | Forbidden dependency check | Can't enforce "message must not import worker" |
| **NEW** | Module group awareness | Detect domain/, ports/, infra/ as layers automatically |

### Target .archlint.yaml

```yaml
rules:
  fan_out:
    threshold: 5
    exclude: [main]    # CLI entry point

  fan_in:
    threshold: 8

  dip:
    check: [domain, ports, app]  # these must have traits

  layers:
    - domain           # layer 0: no internal deps
    - ports            # layer 1: depends on domain only
    - infra            # layer 2: depends on domain + ports
    - app              # layer 2: depends on domain + ports
    - adapters         # layer 2: depends on domain + ports
    - cli              # layer 3: depends on app
    - main             # layer 4: wires everything

  forbidden_deps:
    - from: domain
      to: [infra, app, adapters, cli]
      reason: "domain must not depend on outer layers"
    - from: ports
      to: [infra, app, adapters, cli]
      reason: "ports must not depend on implementations"
    - from: app
      to: [infra]
      reason: "app works through traits, not concrete infra"

  perf:
    max_complexity: 15
    max_nesting: 5
```

---

## Roadmap

### Phase 1: Domain Extraction (no new features)

Вытащить чистую доменную модель из существующего кода:

| What | From | To | Issue |
|------|------|----|-------|
| Message, Envelope, Metadata | `message.rs` | `domain/message.rs` | #138 |
| ContextGraph, Node, Session | `context.rs` | `domain/context.rs` | #138 |
| ExecutionGraph, Step | `graph.rs` (types only) | `domain/graph.rs` | #138 |
| Task, TaskStatus, TaskCriteria | `task.rs` (types only) | `domain/task.rs` | #140 |
| Instance, Transition | `statemachine.rs` (types only) | `domain/statemachine.rs` | #140 |
| InboxMessage | `unified_inbox.rs` (type only) | `domain/inbox.rs` | #142 |
| UserConfig, AgentDef, ModelDef | `config.rs` (types only) | `domain/config.rs` | #144 |

Define ports:
| Trait | Purpose | Issue |
|-------|---------|-------|
| Executor | execute(session) → result | #138 |
| MessageBus | send, recv, register | #139 |
| ContextStore | save, load, list contexts | NEW |
| TaskRepository | task CRUD + claim | #140 |
| StateMachineRepository | SM persistence | #140 |
| InboxRepository | unified inbox | #142 |

**archlint gate:** DIP violations → 0, no domain→infra imports.

### Phase 2: Decomposition (refactor existing code)

| What | Issue |
|------|-------|
| Break up worker::run() → Worker struct with injected deps | #141 |
| Consolidate inbox.rs + unified_inbox.rs → one InboxRepository | #142 |
| Break up main.rs → cli/ + serve.rs | #143 |
| Fix config: extract paths, remove circular deps | #144 |
| Extract ClaudeExecutor from agent.rs | #138 |
| Extract AcpExecutor from acp.rs | #138 |
| Extract UnixBus from bus.rs | #139 |
| Extract FileStores from task.rs, statemachine.rs, inbox | #140 |

**archlint gate:** fan-out ≤7, fan-in ≤8, CC ≤15.

### Phase 3: Testing

| What | Issue |
|------|-------|
| Tests for Executor impls (command building, state transitions) | #145 |
| Tests for Worker (with InMemoryBus + MockExecutor) | #145 |
| Tests for MCP (request/response parsing, tool dispatch) | #145 |
| Integration tests: full pipeline with in-memory infra | #145 |

**archlint gate:** 0 untested modules.

### Phase 4: CI Enforcement

| What |
|------|
| archlint-rs quality gate in CI (block merge on regression) |
| Health score ≥75/100 |
| Auto-generate architecture.yaml on each merge |
| Diff report on PRs (new violations highlighted) |

---

## Principles

1. **Context is the core.** Everything serves context management. Executor is infrastructure. Bus is infrastructure. Only Context is domain.

2. **Каждый модуль — API.** Public types и functions = контракт. Стабильный, лаконичный, расширяемый (Open/Closed). Можно расширять, нельзя ломать.

3. **Domain не знает про infrastructure.** ContextGraph не знает что хранится в JSON файле. Task не знает что лежит в `~/.deskd/tasks/`. Message не знает про Unix sockets.

4. **Зависимости текут внутрь.** Infrastructure → Ports → Domain. Никогда наоборот. main.rs — единственное место где внешнее подключается к внутреннему (DI).

5. **Traits на границах.** Везде где модуль A использует модуль B — через trait. Конкретные типы видит только main.rs.

6. **Тесты без I/O.** InMemoryBus, InMemoryStore, MockExecutor. Unit tests не трогают файловую систему и сокеты.

7. **archlint validates architecture.** Если archlint не может проверить правило — это issue на archlint, не повод отказаться от правила.
