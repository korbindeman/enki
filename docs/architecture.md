# Architecture: Enki — Multi-Agent Coding Orchestrator

A Rust-based process orchestrator that spawns ACP coding agents in isolated filesystem copies of your project. The orchestrator manages processes, state, and presentation — it makes zero LLM API calls.

## Crate Structure

```
crates/
├── core/              # Library crate. Synchronous state machines.
│   ├── orchestrator.rs  # Command/Event state machine — the brain
│   ├── scheduler.rs     # Tier-aware DAG scheduling with concurrency limits
│   ├── dag.rs           # Dependency graph with pause/cancel/cascade
│   ├── monitor.rs       # Worker health monitoring (stale detection, retries)
│   ├── db/              # SQLite schema, CRUD, auto-migration
│   ├── types.rs         # Core types: Id, Task, Execution, Tier, statuses
│   ├── copy.rs          # Filesystem copy-on-write cloning for worker isolation
│   ├── refinery.rs      # Merge queue processor
│   ├── hashline.rs      # Content-addressed line tracking
│   └── agent_runtime.rs # Agent binary resolution and caching
│
├── cli/               # Binary crate. CLI + TUI.
│   ├── main.rs          # Clap command dispatch
│   ├── commands/        # CLI subcommands (init, run, stop, mcp, doctor)
│   │   └── mcp.rs       # MCP stdio server for external tool access
│   └── tui/             # TUI interface
│       └── coordinator.rs  # Thin async adapter over Orchestrator
│
├── acp/               # ACP client library (Agent Client Protocol)
│   └── lib.rs           # Session lifecycle, JSON-RPC over stdio
│
└── tui/               # TUI rendering library (ratatui-based)
    ├── chat.rs          # Chat framework with markdown rendering
    └── indicator.rs     # Status indicators
```

## Key Design Principles

### DAG is the single source of truth

The in-memory DAG (via `Scheduler`) is the authoritative state for what's running, ready, blocked, paused, or cancelled. The SQLite database is **write-behind persistence** — state is written to DB for crash recovery and external visibility, but runtime decisions read from the DAG.

### Synchronous state machines in core

The `Orchestrator`, `Scheduler`, `MonitorState`, and `Dag` are all pure synchronous types. No async, no tokio, no ACP dependency in `core`. Every method is `fn handle(&mut self, cmd) -> Vec<Event>` — trivially testable.

### Coordinator is a thin async adapter

The CLI's `coordinator.rs` owns the tokio select loop, ACP sessions, and TUI channels. It translates async events (worker completions, TUI messages, merge results, timer ticks) into `Orchestrator::Command`s, and executes the resulting `Event`s (spawn workers, kill sessions, queue merges).

### Signal file protocol for cross-process communication

The MCP server runs as a separate process. It writes to the DB and drops JSON signal files in `.enki/events/`. The coordinator's tick loop picks these up via `Command::CheckSignals` and reacts accordingly.

## Orchestrator: Command/Event API

```rust
pub enum Command {
    CreateExecution { steps: Vec<StepDef> },
    CreateTask { title, description, tier },
    WorkerDone(WorkerResult),
    MergeDone(MergeResult),
    RetryTask { task_id },
    Pause(Target),
    Resume(Target),
    Cancel(Target),
    StopAll,
    MonitorTick { workers },
    Recover,
    DiscoverFromDb,
    CheckSignals,
}

pub enum Event {
    SpawnWorker { task_id, title, description, tier, execution_id, step_id, upstream_outputs },
    KillSession { session_id },
    QueueMerge(MergeRequest),
    WorkerCompleted { task_id, title },
    WorkerFailed { task_id, title, error },
    MergeLanded { mr_id, task_id },
    MergeConflicted { mr_id, task_id },
    MergeFailed { mr_id, task_id, reason },
    ExecutionComplete { execution_id },
    ExecutionFailed { execution_id },
    AllStopped { count },
    MonitorCancel { session_id },
    MonitorEscalation(String),
    TaskRetrying { task_id, attempt, max },
    StatusMessage(String),
}

pub enum Target {
    Execution(String),
    Node { execution_id, step_id },
}
```

## Scheduler: Tier-Based Concurrency

The scheduler manages multiple concurrent executions, each with its own DAG. Concurrency limits are per-tier:

```rust
pub struct Limits {
    pub max_light: usize,    // e.g. 5
    pub max_standard: usize, // e.g. 3
    pub max_heavy: usize,    // e.g. 1
}
```

Each task has a complexity tier (light/standard/heavy) that determines which model handles it and how many can run concurrently. The scheduler's `tick()` evaluates ready nodes across all executions and returns `SchedulerAction::Spawn` for tasks that fit within limits.

## DAG: Node States and Transitions

```
Pending → Ready (when all deps satisfied)
Ready → Running (when spawned by scheduler)
Running → WorkerDone (worker finished, merge pending)
WorkerDone → Done (merge landed)
Running/WorkerDone → Failed (worker failure or merge failure)
Pending/Ready/Running → Paused (pause command)
Paused → Ready/Pending (resume command, re-evaluates deps)
Any non-Done → Cancelled (cancel command, cascades to dependents)
Failed → Ready (retry)
```

Pause and cancel can target a single node or an entire execution. Cancel cascades to all transitive dependents.

### Edge Conditions

Dependencies between nodes can specify when they're satisfied:

- **Merged** (default) — dep must be fully Done (worker finished + merge landed)
- **Completed** — dep's worker must have finished (WorkerDone or Done)
- **Started** — dep just needs to be Running (or further)

This allows overlapping execution: a test step can start as soon as the implementation step's worker is running, without waiting for its merge.

## Monitor: Worker Health

Pure state machine that detects stale workers based on last activity time:

- Workers with no ACP update for `STALE_CANCEL_SECS` (default 120s for standard tier) get a cancel signal
- Retry budget: up to `MAX_TASK_RETRIES` (3) per task before blocking
- No duplicate cancel signals (tracks already-cancelled sessions)

## MCP Server

JSON-RPC 2.0 over stdio. Role-based tool filtering (planner gets all tools, worker gets status + list only).

**Tools:**
- `enki_status` — task counts by status
- `enki_task_create` — create standalone task (writes DB + signal file)
- `enki_task_list` — list all tasks
- `enki_execution_create` — create multi-step execution with dependencies
- `enki_task_retry` — retry a failed task
- `enki_pause` — pause an execution or step
- `enki_cancel` — cancel an execution or step
- `enki_stop_all` — stop all running workers

**Signal file format:**
```json
{"type": "execution_created", "execution_id": "exec-..."}
{"type": "task_created", "task_id": "task-..."}
{"type": "pause", "execution_id": "exec-...", "step_id": "optional"}
{"type": "cancel", "execution_id": "exec-...", "step_id": "optional"}
{"type": "stop_all"}
```

## Worker Isolation: Copy-on-Write Clones

Each worker gets a full filesystem copy of the project at `.enki/copies/<task_id>`. Copies include everything — build artifacts, node_modules, .gitignored files — so workers start with a warm build cache. Uses platform-appropriate CoW (`clonefile` on macOS/APFS, `reflink` on Linux/btrfs) for instant, space-efficient clones.

Git is used only for branching (each copy gets a `task/<id>` branch), committing worker output, fetching the branch back to the source repo, and merging via the refinery.

```
.enki/
├── db.sqlite         # Project state
├── copies/           # Worker filesystem copies
│   ├── task-a1b2.../  # One per active worker
│   └── ...
├── events/           # Signal files from MCP
│   └── sig-01J....json
└── logs/
    └── enki.log
```

## Data Flow

```
User types in TUI chat
    │
    ▼
Coordinator agent reasons, calls MCP tools
    │
    ▼
MCP writes to DB + signal file
    │
    ▼
Coordinator tick → CheckSignals → DiscoverFromDb
    │
    ▼
Orchestrator returns SpawnWorker events
    │
    ▼
Coordinator creates CoW copy, spawns ACP session in it
    │
    ▼
Workers execute, stream updates to TUI
    │
    ▼
Worker completes → commit changes → WorkerDone command
    │
    ▼
Orchestrator returns QueueMerge event
    │
    ▼
Refinery fetches branch, merges → MergeDone command
    │
    ▼
Orchestrator advances DAG, spawns downstream workers
```

## Database Schema (Simplified)

```sql
sessions         -- id, started_at, ended_at
tasks            -- id, session_id, title, description, status, tier, assigned_to, copy_path, branch, base_branch, timestamps
executions       -- id, session_id, status, dag, created_at
execution_steps  -- execution_id, step_id, task_id
task_dependencies -- task_id, depends_on
merge_requests   -- id, session_id, task_id, branch, base_branch, status, priority, diff_stats, timestamps
task_outputs     -- task_id, output
messages         -- id, from_addr, to_addr, subject, body, priority, msg_type, thread_id, timestamps
```

Auto-migration: `auto_migrate()` runs on every DB open, parses the schema const, and `ALTER TABLE ADD COLUMN` for anything missing. No version files needed.
