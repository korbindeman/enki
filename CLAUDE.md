# enki

Multi-agent coding orchestrator. Rust 2024 edition workspace.

## Commands

```bash
just run <ARGS>       # cargo run --bin enki -- <ARGS>
just chat             # TUI chat example (enki-tui)
just install          # Build release binary to ~/.cargo/bin
just release          # Tag + push a date-based release (vYYYY.MM.DD)
cargo test -p enki-core  # Core has inline unit + integration tests
cargo build           # Build all crates
```

## Architecture

Four crates, strict dependency direction: `cli` → `tui`, `acp`, `core`. No cycles.

| Crate | Role |
|-------|------|
| `core` | **Pure sync** state machine. Orchestrator, DAG scheduler, SQLite persistence, worktree copy manager, git merge refinery, roles, hashlines. Zero async, zero tokio. |
| `acp` | Async ACP client. Spawns agent subprocesses, manages sessions, routes streaming updates. All internal state is `Rc<RefCell<...>>` — **`!Send`**, must run on a `LocalSet`. |
| `tui` | Sync terminal UI library over raw `crossterm` (not ratatui). Chat framework via `Handler<M>` trait. Optional `markdown` feature for `termimad`+`syntect` rendering. |
| `cli` | The `enki` binary. No args → TUI. `enki mcp` → JSON-RPC stdio server. Houses the coordinator loop (`tokio::select!` on a dedicated OS thread with `current_thread` runtime + `LocalSet`). |

**Data flow:** User → Coordinator (cli) → Orchestrator (core) produces `Vec<Event>` → Coordinator dispatches workers via AgentManager (acp) → Workers call back through MCP tools → Signal files → Coordinator polls on 3s tick.

## Key Patterns

**Hashlines**: `read_text_file` tags each line with `{line_num:>width}:{xxh3_hash}|{content}`. `write_text_file` verifies anchors to detect stale edits. Implemented in `core/src/hashline.rs`.

**Two-phase worker completion**: Worker finishes (`WorkerDone`, frees tier slot) → merge runs → `MergeDone` advances DAG. The scheduler tracks both phases separately for concurrency accounting.

**Signal file IPC**: MCP server writes `.enki/events/sig-*.json`. Coordinator polls and deletes on each tick. No fsnotify.

**`process_events` cascade**: Spawning a worker can fail and produce new events. The coordinator drains in a `while !events.is_empty()` loop.

**`infra_broken` flag**: If worktree creation fails during worker spawn, coordinator auto-fails all subsequent spawns rather than retrying.

**Merger agent flow**: On merge conflict, `MergeNeedsResolution` spawns a separate ACP session with minimal tools working in a shared temp clone. `CleanupGuard` + `std::mem::forget` keeps the temp dir alive during resolution.

## Environment Variables

| Var | Purpose |
|-----|---------|
| `ENKI_BIN` | Path to own binary, injected into all subprocesses |
| `ENKI_DIR` | Project `.enki/` directory for DB + signal files |
| `ENKI_SESSION_ID` | Scopes MCP tool results to current session |
| `CLAUDECODE` | Cleared on agent spawn to prevent nested-session refusal |

## Project Directory (`.enki/`)

```
.enki/
├── db.sqlite          # SQLite (WAL mode), DAG stored as JSON blob
├── copies/<task_id>/  # Git worktrees (one per worker), symlinks to gitignored dirs
├── copies/.merge-*/   # Temp shared clones for merge conflict resolution
├── roles/*.toml       # Project-specific role overrides
├── artifacts/<exec>/<step>.md  # Artifact output mode files
├── events/sig-*.json  # Signal files (IPC)
└── verify.sh          # Optional merge hook (exit non-zero = fail merge)
```

Roles load in cascade: builtins → `~/.enki/roles/*.toml` → `.enki/roles/*.toml`.

## Logs

When the user mentions "the logs" or "check the logs", read `~/.enki/logs/enki.log`. Sessions separated by `══════════════════ SESSION START ══════════════════`. Most recent session is at the bottom.

```bash
tail -n 200 ~/.enki/logs/enki.log          # End of most recent session
grep "ERROR\|WARN" ~/.enki/logs/enki.log   # Find problems quickly
```

Per-agent session logs: `~/.enki/logs/sessions/<label>.log` (timestamped JSON-RPC traffic).

Log levels: `ERROR` = failures, `INFO` = lifecycle events, `DEBUG` = subprocess args, copy paths, prompt sizes, session kills.

## Gotchas

- **`!Send` boundary**: All ACP code uses `Rc<RefCell<...>>`. The coordinator runs on its own OS thread with a `current_thread` runtime + `LocalSet`. Never send ACP types across threads.
- **Architecture doc says "ratatui-based"** — incorrect. TUI uses raw crossterm with a custom canvas.
- **`Abandoned` status**: DB-only state set on session exit for in-flight tasks. Never enters the DAG.
- **DB migrations**: `auto_migrate()` parses schema and `ALTER TABLE ADD COLUMN` for missing columns. No migration files, no downmigrations.
- **Copy manager**: Uses `git worktree add` to create isolated working copies in `.enki/copies/<task_id>`. Symlinks top-level gitignored directories (e.g. `target/`) back into the worktree for build caching. Symlinks are hidden before `git add -A` and restored after to avoid staging cached artifacts.
- **MCP role-based tool access**: `PLANNER_TOOLS` (full), `WORKER_TOOLS`, `WORKER_TOOLS_NO_EDIT`, `MERGER_TOOLS` (minimal).
