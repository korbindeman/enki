# Enki Roadmap

## Current State

### What's Working

**Core engine (88 tests passing):**
- `Orchestrator` — synchronous Command/Event state machine. Handles execution creation, worker lifecycle, merge coordination, pause/resume/cancel, crash recovery, signal file processing.
- `Scheduler` — tier-aware (light/standard/heavy) concurrency with per-tier limits. Manages multiple concurrent DAG executions.
- `Dag` — dependency graph with parallel execution, failure cascade, pause/resume/cancel with transitive cascade.
- `MonitorState` — worker health monitoring with stale detection and retry budgets.
- `Db` — SQLite with WAL mode, auto-migration, simplified schema (tasks, executions, execution_steps, dependencies, agents, merge_requests).
- `WorktreeManager` — git worktree lifecycle: bare repo init, create/remove worktrees, sync, merge (fast-forward + merge commit for diverged histories).
- `Refinery` — merge queue processor.

**CLI:**
- `enki init` — creates `.enki/` directory with db, bare repo
- `enki run <task-id>` — manually runs one task via ACP in a worktree
- `enki stop` — stops all running workers
- `enki mcp` — MCP stdio server with full tool set (status, task CRUD, execution create, pause, cancel, retry, stop)
- `enki doctor` — health checks

**TUI:**
- Chat interface with streaming markdown rendering
- File autocomplete triggered by `@`
- Worker spawn/complete/fail notifications
- Coordinator loop: thin async adapter over Orchestrator
- Event-driven execution via signal files (no poll-based discovery)

### Architecture Simplification (Completed)

The following speculative features were removed to focus the codebase:
- Template system (TOML parser, variable substitution)
- Task groups / convoys
- Typed inter-agent messages
- Memory system (FTS5 tables)
- Usage tracking
- Agent roles enum

Orchestration logic was extracted from the 1,782-line coordinator into a testable synchronous state machine in `core`. The coordinator is now ~1,267 lines — a thin async adapter.

## Roadmap

### Phase 1 — Stability and Visibility

| # | What | Why |
|---|------|-----|
| 1.1 | **Per-worker status in TUI** — sidebar showing each worker's current tool call and latest output | Workers are currently a black box during execution |
| 1.2 | **Merge conflict recovery** — on conflict, leave worktree intact, allow retry with rebase | Conflicts are currently permanent failures |
| 1.3 | **Worker cancellation from TUI** — kill individual workers via keyboard shortcut | Long-running workers can't be stopped individually |

### Phase 2 — Reliability

| # | What | Why |
|---|------|-----|
| 2.1 | **End-to-end integration tests** — spawn real ACP sessions in test worktrees, verify full lifecycle | Core is well-tested but the async glue layer isn't |
| 2.2 | **Graceful shutdown** — on SIGINT, finish in-progress merges, kill workers cleanly, persist state | Currently just drops everything |
| 2.3 | **Rate limit backoff** — detect ACP rate limit errors and queue tasks instead of failing | Running 5+ workers hits Claude Max limits |

### Phase 3 — Polish

| # | What | Why |
|---|------|-----|
| 3.1 | **Project context on startup** — if CWD isn't initialized, prompt to `enki init` | Coordinator is disoriented without project context |
| 3.2 | **`@file` content injection** — inject file content into prompt, not just the path | Currently relies on coordinator choosing to read the file |
| 3.3 | **Execution progress display** — show DAG progress with step status in TUI | Hard to track multi-step execution progress |
