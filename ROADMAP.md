# Enki MVP Roadmap

## Current State

### What's Working

**Core engine:**
- `Db` — full SQLite CRUD for projects, tasks, agents, messages, merge requests, usage, memories (with FTS5)
- `Dag` — dependency graph with parallel execution, failure propagation
- `Template` — TOML workflow templates with variable substitution and cycle detection
- `Scheduler` — tier-aware (light/standard/heavy) concurrency manager (built, tested, not yet used in runtime)
- `WorktreeManager` — git worktree lifecycle: create, remove, list, sync, merge (fast-forward + merge commit for diverged histories)
- `AgentManager` (ACP) — spawns ACP agent processes, manages sessions, auto-approves permissions, handles terminals

**CLI:**
- `enki init` — creates `~/.enki/db.sqlite`
- `enki project add <path>` — registers a git repo, creates a bare clone at `<path>/.enki.git`
- `enki project list`
- `enki task create/list/update-status`
- `enki exec run <project-id> <template.toml> [--var k=v ...]` — renders a template, creates all tasks with dependency wiring, sets leaf tasks to ready
- `enki run <task-id> [--keep]` — manually runs one task via ACP in a new worktree; cleans up on success
- `enki status` — shows project/task count summary

**TUI:**
- Chat interface with streaming markdown rendering
- File autocomplete triggered by `@`
- Worker spawn/complete/fail notifications
- Scrollable output
- Coordinator loop:
  - Starts a dedicated Claude ACP session (coordinator)
  - Spawns workers on a separate `AgentManager` — no TUI callback, zero output mixing
  - Syncs bare repo before each worker branch is created
  - Polls DB every 3 seconds for `status=ready` tasks
  - Enforces tier concurrency limits (1 heavy / 3 standard / 5 light)
  - Promotes blocked tasks to `ready` automatically when their dependencies complete
  - Merges completed branches to main, cleans up worktrees

**Coordinator awareness:**
- Knows about `enki exec` and when to use templates vs single tasks
- Understands dependency promotion is automatic (does not need to sequence tasks manually)

### What's Still Missing

**Usability gaps:**

1. **No per-worker output visibility** — Workers run silently. The TUI shows spawn/complete toasts but nothing mid-task. You can't tell if a worker is stuck, running tests, or making progress.

2. **No merge conflict recovery** — When `merge_branch` fails (parallel workers touch the same file), the task is marked `Failed` with no recovery path. The worktree is deleted. There is no retry flow.

3. **Coordinator is CWD-dependent** — The TUI uses `std::env::current_dir()` as the workspace root. If you open enki outside a registered project, the coordinator agent is disoriented with no guidance.

**Unbuilt features (schema exists, no implementation):**

4. **Memory system** — `memories` table with FTS5 exists in schema. Nothing reads or writes to it. Agents have no cross-session memory.

5. **Usage tracking** — `usage` table exists. Nothing writes token counts or durations. Costs are invisible.

**Minor:**

6. **Worker cancellation** — Once a worker is spawned, there's no way to kill it from the TUI.

7. **`@file` content injection** — The `@` autocomplete pastes a path string. The coordinator agent has to decide whether to read the file. No explicit content injection.

---

## MVP Definition

A user opens enki in a git repo, describes a coding task in natural language, watches parallel worker agents implement it in isolated branches, and gets code merged back to main — reliably, with enough visibility to trust what's happening.

---

## Roadmap

### ✅ Phase 1 — Foundation fixes *(done)*

| # | What | Status |
|---|------|--------|
| 1.1 | `WorktreeManager::sync()` — fetch origin before branching so workers start from current code | Done |
| 1.2 | Separate `AgentManager` for workers — prevents worker ACP output from polluting the coordinator stream | Done |
| 1.3 | `enki run` worktree cleanup — removes worktree on success; `--keep` flag to skip | Done |

### ✅ Phase 2 — Scheduler wired up *(done)*

| # | What | Status |
|---|------|--------|
| 2.1 | Dependency promotion + tier-aware spawning — `promote_unblocked_tasks` advances the DAG on completion; tier counts enforced before each spawn | Done |
| 2.2 | `enki exec run <project-id> <template.toml> [--var k=v ...]` — bridges the template pipeline to the live system | Done |
| 2.3 | Coordinator system prompt update — teaches coordinator about `enki exec` and automatic dependency promotion | Done |

### Phase 3 — Visibility and stability

Makes it trustworthy enough to use on real work.

| # | What | Why |
|---|------|-----|
| 3.1 | **Per-worker status in TUI** — a sidebar or inline section showing each active worker's latest output line and current tool call | Without this, parallel workers are a black box |
| 3.2 | **Merge conflict recovery** — on conflict, leave worktree intact, mark task `blocked`, expose `enki task retry <id>` to rebase and re-run | Conflicts are currently permanent failures with no escape hatch |
| 3.3 | **Project context on TUI startup** — if CWD isn't a registered project, prompt to add it or select from the registered list | Coordinator frequently mis-oriented without this |

### Phase 4 — Polish

Makes it feel complete.

| # | What | Why |
|---|------|-----|
| 4.1 | **Worker cancellation** — `enki task cancel <id>` kills the ACP session and cleans up the worktree | Long-running workers can't be stopped today |
| 4.2 | **Memory writes** — coordinator writes key decisions and context to the `memories` table; workers query it on startup via FTS5 | Agents start each session cold with no project context |
| 4.3 | **Usage tracking** — write token counts and durations to `usage` after each session; show cumulative cost in `enki status` | Running completely blind on costs |
| 4.4 | **`@file` content injection** — when the user types `@path/to/file`, inject the file's content into the prompt rather than just the path string | Currently relies on the coordinator agent choosing to read the file |
