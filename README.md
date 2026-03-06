# enki

Multi-agent coding orchestrator. Spawns [ACP](https://github.com/agentclientprotocol/agent-client-protocol) agents in isolated copies of your project, runs them as a DAG, and merges their work back.

Enki doesn't make LLM API calls. It orchestrates agent processes and manages state. Currently only supports Claude Code. More agents coming, plus a way to bring your own.

## How it works

1. You describe work in the TUI. A coordinator agent splits it into tasks with dependencies.
2. Each task runs in a copy-on-write clone of your project (build artifacts, node_modules, everything).
3. Agents run tasks in parallel, respecting the dependency graph and concurrency limits.
4. Finished work gets merged back into your working directory.

Works in any folder, git or not. In a git repo, enki commits each task on a branch and merges through a queue. Without git, it uses git internally to track changes but doesn't leave a repo behind.

## Requirements

- [Git](https://git-scm.com/)
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code): the agent enki orchestrates
- [Node.js](https://nodejs.org/): used to install and run the ACP agent
- [Rust](https://rustup.rs/): if building from source

## Install

```bash
brew install korbindeman/tap/enki
```

Or build from source:

```bash
cargo install --path crates/cli
```


## Usage

```bash
enki
```

Opens the TUI. Sets up the project on first run.

## Architecture

Rust workspace, four crates:

| Crate | Role |
|-------|------|
| `core` | Sync state machines: orchestrator, DAG scheduler, monitor, DB, merge queue |
| `acp` | ACP client for agent communication |
| `tui` | Terminal rendering (crossterm) with streaming markdown |
| `cli` | The binary: CLI commands, TUI, async coordinator |

All orchestration logic is in sync state machines (`Orchestrator`, `Scheduler`, `Dag`, `MonitorState`). The coordinator is a thin async layer that translates between tokio events and `Command`/`Event` pairs.

## Acknowledgements

- [Gastown](https://github.com/steveyegge/gastown) by Steve Yegge
- [Agent Client Protocol](https://github.com/agentclientprotocol/agent-client-protocol)

## License

[MIT](LICENSE)
