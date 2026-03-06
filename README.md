# enki

Multi-agent coding orchestrator. Spawns [ACP](https://github.com/agentclientprotocol/agent-client-protocol) coding agents in isolated filesystem copies of your project, schedules them as a DAG, and merges their work back to your branch.

Enki makes zero LLM API calls itself. It orchestrates agent processes and manages state. Currently only supports Claude Code. More agents are coming, along with a way to bring your own ACP-compatible agent.

## How it works

1. You describe work in the TUI chat. A coordinator agent breaks it into tasks with dependencies.
2. Each task gets an isolated copy-on-write clone of your project (including build artifacts, node_modules, etc.).
3. ACP agents execute tasks in parallel, respecting the dependency DAG and per-tier concurrency limits.
4. Completed work is merged back into your working directory.

Enki works in any folder, git or not. If your project is a git repo, enki commits each task's changes on a branch and merges them through a queue. If it's not a git repo, enki uses git internally to track and merge changes, but doesn't leave a repo behind.

## Install

```
cargo install --path crates/cli
```

Requires an ACP-compatible agent (e.g. [`@zed-industries/claude-agent-acp`](https://github.com/zed-industries/claude-agent-acp)).

## Usage

```bash
enki
```

Launches the TUI. Initializes the project automatically on first run.

## Architecture

Rust workspace with four crates:

| Crate | Role |
|-------|------|
| `core` | Synchronous state machines: orchestrator, DAG scheduler, monitor, DB, merge queue |
| `acp` | ACP client library for agent communication |
| `tui` | Terminal rendering (crossterm-based) with streaming markdown |
| `cli` | Binary: CLI commands, TUI interface, async coordinator |

All orchestration logic lives in synchronous state machines (`Orchestrator`, `Scheduler`, `Dag`, `MonitorState`). The coordinator is a thin async adapter that translates between tokio events and `Command`/`Event` pairs.

## Acknowledgements

- [Gastown](https://github.com/steveyegge/gastown) by Steve Yegge — reference for agent orchestration patterns
- [Agent Client Protocol](https://github.com/agentclientprotocol/agent-client-protocol) — the protocol enki uses to talk to agents

## License

[MIT](LICENSE)
