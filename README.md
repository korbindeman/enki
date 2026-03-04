# enki

Multi-agent coding orchestrator. Spawns [ACP](https://github.com/zed-industries/agent-client-protocol) coding agents in isolated filesystem copies of your project, manages their lifecycle through a DAG scheduler, and merges their work back to your branch.

Enki makes zero LLM API calls itself — it orchestrates agent processes, manages state, and handles presentation.

## How it works

1. You describe work in the TUI chat. A coordinator agent breaks it into tasks with dependencies.
2. Each task gets an isolated copy-on-write clone of your project (including build artifacts, node_modules, etc.).
3. ACP agents execute tasks in parallel, respecting the dependency DAG and per-tier concurrency limits.
4. Completed work is committed on a task branch, fetched back, and merged through a merge queue.

## Install

```
cargo install --path crates/cli
```

Requires an ACP-compatible agent (e.g. [`@zed-industries/claude-agent-acp`](https://github.com/zed-industries/claude-agent-acp)).

## Usage

```bash
# Initialize enki in your project
enki init

# Launch the TUI (default — just run `enki`)
enki

# Check workspace status
enki status

# Run a single task manually
enki run <task-id>

# Diagnose project health
enki doctor
```

## Architecture

Rust workspace with four crates:

| Crate | Role |
|-------|------|
| `core` | Synchronous state machines — orchestrator, DAG scheduler, monitor, DB, merge queue |
| `acp` | ACP client library for agent communication |
| `tui` | Terminal rendering library (ratatui-based) with streaming markdown |
| `cli` | Binary — CLI commands, TUI interface, async coordinator |

The core design principle: all orchestration logic lives in pure, synchronous state machines (`Orchestrator`, `Scheduler`, `Dag`, `MonitorState`). The coordinator is a thin async adapter that translates between tokio events and `Command`/`Event` pairs.

See [docs/architecture.md](docs/architecture.md) for the full design and [docs/roadmap.md](docs/roadmap.md) for planned work.

## License

[MIT](LICENSE)
