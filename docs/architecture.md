# Architecture: Enki — Multi-Agent Orchestrator

A Rust-based process orchestrator for ACP coding agents. Inspired by Gastown's proven patterns, with targeted improvements from Enki's architectural ideas.

## Reference Implementation

Gastown ([github.com/steveyegge/gastown](https://github.com/steveyegge/gastown)) is our primary reference. Licensed under MIT, which permits reading, adapting, and deriving from their source. During implementation, consult their Go source for:

- Git worktree lifecycle (`internal/worktree/`)
- Convoy/task group state machines (`internal/convoy/`)
- Agent role lifecycle and hook system (`internal/agent/`)
- Merge queue / refinery logic (`internal/refinery/`)
- Formula parsing and execution (`internal/formula/`)
- Mail system patterns (`internal/mail/`)

Note: Gastown uses tmux as the process host for agents. We use ACP (Agent Communication Protocol) instead — agents run as subprocesses communicating over stdio via JSON-RPC 2.0. Their tmux patterns (`internal/tmux/`) are not referenced.

We are reimplementing in Rust, not forking. Reference their logic and state machines, but the code is ours. Include MIT attribution in THIRD_PARTY_LICENSES if we port any substantial logic directly.

Beads ([github.com/steveyegge/beads](https://github.com/steveyegge/beads)) is also MIT licensed. We don't depend on it (task management is built-in), but reference their issue data model and git-backed storage patterns.

## Guiding Principle

This tool makes zero LLM API calls. Every intelligent decision is made by an ACP agent (Claude Code, Codex, Gemini CLI, etc.). The orchestrator manages processes, state, and presentation.

## Concept Mapping from Gastown

| Gastown | Ours | Notes |
|---------|------|-------|
| Town (`~/gt/`) | Workspace | Root directory containing all projects and config |
| Rig | Project | Git repository container with agent workspaces |
| Mayor | Coordinator | ACP session with orchestrator tools. Primary user interface. |
| Polecat | Worker | Ephemeral ACP session per task, isolated via git worktree |
| Witness | Monitor | Watches worker health, detects stuck agents |
| Refinery | Merger | Merge queue processor, PR review |
| Bead / Issue | Task | Unit of work. Stored in SQLite, not git-backed files. |
| Convoy | TaskGroup | Bundle of related tasks assigned together |
| Hook | Worktree + State | Git worktree for code isolation, SQLite row for state |
| Formula | Template | TOML-defined workflow with steps and dependencies |
| Molecule | Execution | Live instantiation of a template, tracked as a DAG |
| Mail | Message | Inter-agent communication channel |
| `bd` (beads CLI) | Integrated | No separate tool. Task management built into the orchestrator. |
| `gt` CLI | `enki` CLI + TUI | Single binary, both CLI commands and rich TUI mode |

## What We Keep from Gastown (Proven)

**Role separation.** Coordinator plans, workers execute, monitor watches, merger handles PRs. Each role is an ACP session with role-specific context injected via system prompt / CLAUDE.md equivalent.

**Git worktree isolation.** Each worker gets its own worktree off a shared bare repo. No branch conflicts. Workers can see each other's branches without pushing to remote.

**ACP as agent transport.** Agent sessions are ACP subprocesses communicating over stdio (JSON-RPC 2.0). The orchestrator spawns agent processes, creates sessions with `session/new` (passing the worktree path as `cwd`), sends prompts via `session/prompt`, and receives streaming updates via `session/update` notifications. No tmux — ACP provides the full session lifecycle natively.

**Formula/template system.** TOML-defined workflows with steps and `needs` dependencies. Users write templates for repeatable processes.

**Convoy/task group model.** Related tasks are grouped. Progress is tracked per-group. Groups land when all tasks complete.

**Propulsion principle.** Agents wake up and execute what's on their hook. No polling, no waiting for commands.

**Crash recovery.** State is in SQLite + git worktrees. If an agent dies, a new one can pick up from the last known state.

## What We Change (Enki-Inspired Improvements)

### 1. SQLite for Orchestration State (not git-backed files)

Gastown stores all state as beads in git. This means scanning directories and parsing files for status queries. We use SQLite for orchestration (tasks, task groups, agent state, messages) and git only for code worktrees.

Benefits: proper queries, indexes, transactions, fast dashboard rendering, no git merge conflicts on state updates.

### 2. DAG Execution for Templates

Gastown's formulas have `needs` dependencies but polecats execute steps linearly. We execute the dependency graph properly: independent steps run in parallel on separate workers.

A template's steps are a DAG. When instantiated into an execution, each step becomes a node. The scheduler evaluates ready nodes (all dependencies satisfied) and assigns them to workers. Multiple workers can execute independent branches concurrently.

This is not Enki's full graph runtime (no computational primitives, no event resolution). Nodes are work items, edges are dependencies. The scheduler is a loop:

```
loop {
    for node in execution.nodes where status == Ready {
        if all dependencies satisfied {
            claim tmux slot
            spawn ACP session
            inject context (objective, inputs from upstream, project state)
            mark Running
        }
    }
    
    for node in execution.nodes where status == Running {
        poll ACP session status
        if completed { mark Done, evaluate downstream }
        if failed { handle (retry / escalate / abort) }
        if stuck { notify monitor }
    }
    
    sleep(poll_interval)
}
```

### 3. TUI as Primary Interface

Gastown's primary interface is `gt mayor attach` (entering the Mayor's tmux session). We provide a ratatui TUI with:

- **Dashboard panel:** Active task groups, worker status, queue depth
- **Chat panel:** Conversation with the coordinator agent
- **Agent panel:** View any agent's ACP session activity (streaming output, tool calls, status)
- **Log panel:** Event stream (task created, assigned, completed, failed)

The coordinator agent is an ACP session. The user talks to it through the TUI's chat panel.

### 4. Richer Agent Context Injection

Gastown injects context via CLAUDE.md/AGENTS.md files in the worktree. We supplement this with structured context passed through the ACP session prompt:

- Task objective and acceptance criteria
- Outputs from upstream tasks in the DAG
- Relevant project state (recent git log, open PRs, CI status)
- Messages from other agents or the coordinator

### 5. Typed Inter-Agent Messages

Gastown's mail is plaintext. We use typed messages stored in SQLite:

- `StatusUpdate { agent, task, progress, summary }`
- `Escalation { agent, task, reason, context }`
- `Handoff { from_agent, to_agent, task, context }`
- `Review { agent, task, verdict, comments }`

The coordinator and monitor can query these structured messages to make better decisions.

## Architecture

### Crate Structure

```
crates/
├── core/              # Library crate. Embeddable orchestrator.
│   ├── workspace.rs   # Workspace/project management
│   ├── task.rs        # Task, TaskGroup, Template types
│   ├── dag.rs         # DAG structure, execution tracking
│   ├── scheduler.rs   # Node readiness evaluation, assignment
│   ├── agent.rs       # Agent state tracking (SQLite), delegates I/O to acp crate
│   ├── worktree.rs    # Git worktree create/cleanup
│   ├── db.rs          # SQLite schema and queries
│   ├── message.rs     # Typed inter-agent messages
│   └── template.rs    # TOML template parsing, instantiation
│
├── cli/               # Binary crate. CLI + TUI.
│   ├── main.rs        # Clap command dispatch
│   ├── commands/      # CLI subcommands (init, add, status, ...)
│   └── tui/           # Ratatui TUI
│       ├── app.rs     # App state, event loop
│       ├── dashboard.rs
│       ├── chat.rs
│       ├── agents.rs
│       └── log.rs
│
└── acp/               # ACP client library (wraps agent-client-protocol crate)
    ├── session.rs     # Session create/prompt/update/cancel
    ├── process.rs     # Agent subprocess lifecycle (spawn, initialize, shutdown)
    └── types.rs       # Protocol types, update routing
```

The `core` crate is the embeddable library. It has no TUI dependency. Anyone can build a different frontend (web dashboard, Dioxus GUI, editor plugin) on top of it.

The `cli` crate is the user-facing binary. `brew install enki` gives you this.

### Data Model (SQLite)

```sql
-- Projects managed by the orchestrator
CREATE TABLE projects (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    repo_url    TEXT,
    local_path  TEXT NOT NULL,
    bare_repo   TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Individual work items
CREATE TABLE tasks (
    id          TEXT PRIMARY KEY,  -- prefix-xxxxx format
    project_id  TEXT NOT NULL REFERENCES projects(id),
    title       TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'open',
        -- open | ready | running | done | failed | blocked
    assigned_to TEXT,              -- agent session id
    worktree    TEXT,              -- path to git worktree
    branch      TEXT,              -- git branch name
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Groups of related tasks
CREATE TABLE task_groups (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active',
        -- active | landed | aborted
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE task_group_members (
    group_id    TEXT NOT NULL REFERENCES task_groups(id),
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (group_id, task_id)
);

-- DAG edges (task dependencies)
CREATE TABLE task_dependencies (
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    depends_on  TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on)
);

-- Execution instances of templates
CREATE TABLE executions (
    id          TEXT PRIMARY KEY,
    template    TEXT NOT NULL,     -- template name
    group_id    TEXT REFERENCES task_groups(id),
    status      TEXT NOT NULL DEFAULT 'running',
    vars        TEXT,              -- JSON: template variables
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Maps execution steps to tasks
CREATE TABLE execution_steps (
    execution_id TEXT NOT NULL REFERENCES executions(id),
    step_id      TEXT NOT NULL,    -- step id from template
    task_id      TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (execution_id, step_id)
);

-- Agent sessions
CREATE TABLE agents (
    id          TEXT PRIMARY KEY,
    role        TEXT NOT NULL,     -- coordinator | worker | monitor | merger
    project_id  TEXT REFERENCES projects(id),
    acp_session  TEXT,             -- ACP session id
    pid          INTEGER,          -- Agent subprocess PID
    status      TEXT NOT NULL DEFAULT 'idle',
        -- idle | busy | stuck | dead
    current_task TEXT REFERENCES tasks(id),
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen   TEXT
);

-- Typed messages between agents
CREATE TABLE messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    type        TEXT NOT NULL,
        -- status_update | escalation | handoff | review | info
    from_agent  TEXT REFERENCES agents(id),
    to_agent    TEXT,              -- NULL = broadcast
    task_id     TEXT REFERENCES tasks(id),
    payload     TEXT NOT NULL,     -- JSON
    read        INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Token usage tracking
CREATE TABLE usage (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id    TEXT REFERENCES agents(id),
    task_id     TEXT REFERENCES tasks(id),
    model       TEXT NOT NULL,
    input_tokens  INTEGER,
    output_tokens INTEGER,
    duration_ms   INTEGER,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Memory tables (memories, memories_fts) defined in Memory section below
```

### ACP Integration

Each agent role maps to an ACP session with specific capabilities:

**Coordinator:**
- Spawned at startup, long-lived
- ACP session receives orchestrator state as context with each prompt
- The coordinator's "tools" are CLI commands it can run: task creation, assignment, status queries
- User messages from TUI chat panel are forwarded as ACP prompts
- ACP `session/update` notifications stream back to the TUI

**Worker:**
- Spawned on demand when a task is assigned
- Short-lived: one task, then session ends
- ACP session prompt includes: task objective, upstream outputs, project context
- Runs in its own git worktree
- On completion: orchestrator captures output, updates task status, cleans up

**Monitor:**
- Long-lived, per-project
- Periodically prompted with current agent/task state
- Can send nudge messages to stuck workers
- Escalates to coordinator when intervention is needed

**Merger:**
- Long-lived, per-project
- Prompted when a worker completes and a PR is ready
- Reviews changes, runs checks, merges or requests changes

### Template Format

```toml
# templates/shiny.toml
name = "shiny"
description = "Design before code, review before ship"

[vars.feature]
description = "Feature to implement"
required = true

[[steps]]
id = "design"
title = "Design"
description = "Think about architecture for {{feature}}"

[[steps]]
id = "implement"
title = "Implement"
description = "Implement {{feature}} based on the design"
needs = ["design"]

[[steps]]
id = "test"
title = "Test"
description = "Write tests for {{feature}}"
needs = ["design"]  # can run parallel with implement

[[steps]]
id = "review"
title = "Review"
description = "Review implementation and tests for {{feature}}"
needs = ["implement", "test"]  # waits for both

[[steps]]
id = "merge"
title = "Merge"
description = "Merge {{feature}} to main"
needs = ["review"]
```

Instantiating this template creates 5 tasks with proper dependency edges. The scheduler runs `design` first, then `implement` and `test` in parallel, then `review` after both complete, then `merge`.

### TUI Layout

```
┌─────────────────────────────────────────────────────────┐
│  [Dashboard]  [Chat]  [Agents]  [Log]          project ▾│
├───────────────────────────┬─────────────────────────────┤
│                           │                             │
│  Task Groups              │  Chat with Coordinator      │
│  ─────────                │  ─────────────────────      │
│  🚚 Auth System [3/5]    │                             │
│    ✅ design              │  > Build an auth system     │
│    🔄 implement (agent-3) │    with JWT and OAuth        │
│    🔄 test (agent-4)      │                             │
│    ⏳ review              │  I'll create a task group   │
│    ⏳ merge               │  with 5 steps using the     │
│                           │  shiny template...          │
│  🚚 Bug Fixes [1/2]      │                             │
│    ✅ fix-login           │  Created group "Auth        │
│    🔄 fix-signup (agent-5)│  System" with 5 tasks.      │
│                           │  Design step starting now.  │
│  Agents                   │                             │
│  ──────                   │                             │
│  🟢 coordinator           │                             │
│  🟢 agent-3 (implement)  │                             │
│  🟢 agent-4 (test)       │                             │
│  🟢 agent-5 (fix-signup) │  ┌─────────────────────────┐│
│  🟡 monitor              │  │ > _                     ││
│                           │  └─────────────────────────┘│
└───────────────────────────┴─────────────────────────────┘
```

### Process Flow

```
User types in TUI chat
    │
    ▼
TUI sends message to coordinator ACP session
    │
    ▼
Coordinator reasons, outputs tool calls
(e.g. "create tasks", "use shiny template", "assign to workers")
    │
    ▼
Orchestrator intercepts tool outputs, executes them:
    ├── Creates tasks in SQLite
    ├── Creates task group
    ├── Inserts dependency edges
    ├── Evaluates DAG for ready nodes
    ├── For each ready node:
    │     ├── Creates git worktree
    │     ├── Spawns ACP agent subprocess
    │     ├── Creates ACP session (cwd = worktree path)
    │     ├── Sends prompt with task context
    │     └── Marks task as running
    └── Updates TUI
    │
    ▼
Workers execute in parallel
    │
    ▼
Scheduler polls ACP sessions for updates
    ├── Streams progress to TUI
    ├── On completion: evaluate downstream nodes
    ├── On failure: retry / escalate
    └── On stuck: monitor nudges
    │
    ▼
All tasks done → task group lands
    ├── Merger processes PRs
    └── Coordinator notified
```

## Usage Awareness

The target is Claude Max (fixed subscription, rate-limited) not pay-per-token API. The optimization goal is throughput efficiency: use the smallest model that can handle each task well.

### Model Routing

Each task gets a complexity tier that determines which model handles it:

| Tier | Model | Examples |
|------|-------|---------|
| Light | Haiku | Rename refactors, simple test generation, formatting fixes, boilerplate scaffolding, documentation updates |
| Standard | Sonnet | Feature implementation from clear spec, bug fixes with repro steps, test writing for complex logic, code review |
| Heavy | Opus | Architectural decisions, ambiguous requirements, cross-cutting refactors, debugging without clear repro |

The coordinator assigns tiers when creating tasks. The template system can also specify tiers per step:

```toml
[[steps]]
id = "design"
title = "Design"
description = "Think about architecture for {{feature}}"
tier = "heavy"  # This needs deep reasoning

[[steps]]
id = "implement"
title = "Implement"
description = "Implement {{feature}} based on the design"
tier = "standard"  # Clear spec from design step

[[steps]]
id = "test"
title = "Test"
description = "Write tests for {{feature}}"
tier = "light"  # Tests from clear implementation
```

ACP agents configure the model at session level. When spawning a worker, the orchestrator passes the appropriate model configuration. For Claude Code specifically, this maps to the `--model` flag or equivalent ACP config.

### Token Tracking

Even on Max, we want visibility into consumption. The orchestrator tracks per-task and per-session:

```sql
CREATE TABLE usage (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id    TEXT REFERENCES agents(id),
    task_id     TEXT REFERENCES tasks(id),
    model       TEXT NOT NULL,           -- haiku-4.5, sonnet-4.5, opus-4.5
    input_tokens  INTEGER,
    output_tokens INTEGER,
    duration_ms   INTEGER,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
```

This feeds the TUI dashboard: total tokens today, tokens per task group, model distribution. The user sees if something is burning through Opus on trivial work.

### Rate Limit Awareness

The scheduler respects throughput limits. If we're hitting rate limits on a tier, the scheduler backs off or queues tasks. Concurrent worker count is tunable:

```toml
# config.toml
[limits]
max_workers = 5
max_heavy = 1         # Only 1 Opus task at a time
max_standard = 3
max_light = 5         # Haiku is cheap, run more
backoff_seconds = 30  # Wait time on rate limit
```

### Task Decomposition Strategy

The coordinator is explicitly prompted to decompose work into small, focused beads. This is both a token optimization (smaller context per task) and a quality optimization (agents perform better on narrow, well-defined tasks). The coordinator's system prompt includes guidance like:

- Prefer 5 small tasks over 1 large task
- Each task should be completable in a single focused session
- Tasks should have clear, verifiable acceptance criteria
- If a task needs Opus, consider whether it can be split so only the ambiguous part needs Opus

## Memory (Embedded, SQLite + FTS5)

Persistent memory across sessions. No external dependencies, no embedding model, no sidecar. Pure SQLite with FTS5 full-text search, embedded in the binary.

Memories are short factual statements: "This project uses thiserror, not anyhow", "Auth module tests were flaky due to race conditions", "User prefers explicit error types."

### Memory Scopes

| Scope | Contents | Injected Into |
|-------|----------|---------------|
| Global | User preferences, coding style, tool preferences | All agents |
| Project | Architecture decisions, conventions, known issues, dependency choices | All agents in that project |
| Task | Relevant past work, related bug history, file-specific notes | Worker assigned to task |

### Retrieval Strategy

No embedding model needed. Retrieval combines three signals:

1. **Full-text search (FTS5):** Query memory content against task description, file paths, and keywords. Technical terms match precisely: "thiserror" matches "thiserror", "auth" matches "auth module".

2. **Path scoping:** Memories are tagged with relevant file paths or directories. When a worker is assigned to files in `src/auth/`, all memories tagged with those paths are included.

3. **Recency + relevance ranking:** FTS5's BM25 ranking combined with recency weighting. Recent memories about the same module are more relevant than old ones.

```sql
-- Memory storage
CREATE TABLE memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    scope       TEXT NOT NULL,        -- global | project | task
    project_id  TEXT REFERENCES projects(id),
    content     TEXT NOT NULL,        -- The actual memory text
    tags        TEXT,                 -- JSON array: file paths, module names, topics
    source_task TEXT REFERENCES tasks(id),  -- Task that produced this memory
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    accessed_at TEXT                  -- Last time this memory was retrieved
);

-- FTS5 virtual table for full-text search
CREATE VIRTUAL TABLE memories_fts USING fts5(
    content,
    tags,
    content='memories',
    content_rowid='id'
);

-- Triggers to keep FTS in sync
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, tags) VALUES (new.id, new.content, new.tags);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, tags) VALUES('delete', old.id, old.content, old.tags);
END;
```

### Retrieval Query

```sql
-- Find relevant memories for a task touching src/auth/ and src/middleware/
SELECT m.*, rank
FROM memories_fts f
JOIN memories m ON m.id = f.rowid
WHERE memories_fts MATCH ?  -- task keywords
  AND m.scope IN ('global', 'project')
  AND (m.project_id IS NULL OR m.project_id = ?)
ORDER BY rank * 0.7 + (julianday('now') - julianday(m.created_at)) * -0.3
LIMIT 20;

-- Plus: all memories tagged with relevant paths
SELECT * FROM memories
WHERE json_each.value IN ('src/auth/', 'src/middleware/')
  AND scope IN ('global', 'project');
```

### Integration Points

**Before agent prompt:** The orchestrator queries memories by task keywords + file paths and injects the top results into the ACP session context.

**After task completion:** The worker agent is prompted to output learnings in a structured format. The orchestrator parses and stores them:

```
[MEMORY] src/auth/middleware.rs: Rate limiting uses a token bucket, not sliding window
[MEMORY] project: Integration tests require Redis running on port 6379
```

**Coordinator context:** Project-scoped memories are included in every coordinator prompt so planning decisions improve over time.

### Memory Lifecycle

- **Creation:** Automatic (post-task extraction) or manual (user/coordinator adds explicitly)
- **Deduplication:** Before inserting, FTS5 search for near-duplicates. If a highly similar memory exists, update it instead.
- **Decay:** Memories that haven't been accessed in N days get lower retrieval priority. Never auto-deleted, but effectively fade.
- **User control:** TUI provides a memory browser. User can view, edit, delete memories per scope.

## Merge Queue

Full merge queue in v1. This is critical for multi-agent work: when 3 workers finish around the same time, their branches need to land cleanly on main without conflicts or broken CI. Reference Gastown's refinery (`internal/refinery/`) for the core state machine.

### How Gastown Does It

The refinery is a Claude Code session on a worktree pointed at main. When a polecat finishes (`gt done`), it pushes its branch and creates a merge request (MR) bead. The refinery picks up MRs in order, rebases onto current main, runs tests, merges to main, pushes, and closes the MR and source issue.

Key insight from Gastown: refinery and polecats share a bare repo (`.repo.git`), so they can see each other's branches without pushing to remote. This is local-only merge queue processing.

### Our Design

Same core pattern, but with SQLite state tracking and explicit queue ordering.

**Lifecycle:**

```
Worker completes task
    │
    ▼
Worker commits to feature branch, pushes to shared bare repo
    │
    ▼
Orchestrator creates merge_request in SQLite (status: queued)
    │
    ▼
Merger agent picks up next queued MR
    │
    ▼
Rebase feature branch onto current main
    ├── Clean rebase → continue
    └── Conflict → mark conflicted, notify coordinator
    │
    ▼
Run verification (build, tests, lint)
    ├── Pass → continue
    └── Fail → mark failed, notify coordinator
    │
    ▼
Review diff (merger agent evaluates: does this look right?)
    ├── Approve → continue
    └── Request changes → mark needs_changes, send message to coordinator
    │
    ▼
Fast-forward merge to main
    │
    ▼
Push main to remote
    │
    ▼
Mark MR as merged, mark source task as done
    │
    ▼
If more MRs queued, process next (rebase will include previous merges)
```

**Queue processing is serial.** One merge at a time per project. This avoids the complexity of speculative parallel merge trains (GitLab-style) which is overkill for our scale. Serial processing with fast feedback is simpler and sufficient for 5-10 concurrent workers.

### Schema

```sql
CREATE TABLE merge_requests (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    group_id    TEXT REFERENCES task_groups(id),
    branch      TEXT NOT NULL,
    base_branch TEXT NOT NULL DEFAULT 'main',
    status      TEXT NOT NULL DEFAULT 'queued',
        -- queued | processing | rebasing | verifying | reviewing
        -- | merged | conflicted | failed | needs_changes
    priority    INTEGER NOT NULL DEFAULT 2,  -- 1=urgent, 2=normal, 3=low
    diff_stats  TEXT,             -- JSON: files changed, insertions, deletions
    review_note TEXT,             -- Merger agent's review comment
    queued_at   TEXT NOT NULL DEFAULT (datetime('now')),
    started_at  TEXT,
    merged_at   TEXT
);
```

### Merger Agent

The merger is a long-lived ACP session per project. It runs on the refinery worktree (checked out to main, shares bare repo with workers). Its prompt includes:

- Current queue state
- The diff to review
- Project conventions (from memory/conventions file)
- Test output from verification step

The merger agent is responsible for:

1. **Code review:** Does the diff match the task description? Are there unnecessary changes? Does it follow conventions?
2. **Conflict resolution:** For trivial conflicts (non-overlapping changes in the same file), attempt auto-resolution. For real conflicts, escalate.
3. **Verification interpretation:** If tests fail, does the failure relate to this change or is it a pre-existing flaky test?

The merger runs at **standard tier** (Sonnet). It doesn't need Opus because the scope is narrow: review one diff, check one test run, merge or reject.

### Verification Steps

Configurable per project in project config:

```toml
# project config
[merge]
base_branch = "main"

[merge.verify]
build = "cargo build"
test = "cargo test"
lint = "cargo clippy -- -D warnings"
format_check = "cargo fmt --check"

[merge.options]
auto_push_remote = true       # Push main to remote after merge
delete_branch_after_merge = true
require_review = true          # Merger agent reviews before merge
allow_auto_merge_on_pass = false  # If true, skip review for green CI
```

### Conflict Handling

When rebase fails:

1. Merger agent examines the conflict markers
2. If the conflict is trivial (same file, different sections), auto-resolve
3. If the conflict is substantive, mark MR as `conflicted` and notify the coordinator
4. Coordinator can: re-assign the original worker to resolve, assign a new worker, or ask the user

### Task Group Landing

A task group "lands" when all its MRs are merged. The orchestrator tracks this:

```sql
-- Check if all tasks in a group are merged
SELECT tg.id, tg.name,
    COUNT(*) as total,
    COUNT(CASE WHEN t.status = 'done' THEN 1 END) as done
FROM task_groups tg
JOIN task_group_members tgm ON tg.id = tgm.group_id
JOIN tasks t ON tgm.task_id = t.id
WHERE tg.status = 'active'
GROUP BY tg.id
HAVING total = done;
-- These groups can be marked as 'landed'
```

When a group lands, the coordinator is notified and can report to the user.

## Coding Ethos

Default conventions injected into every agent's context, optimized for how AI agents actually work well. These are the project's CLAUDE.md equivalent, shipped as sensible defaults that users can override.

### Core Principles

**Test-Driven Development.** Write the test first, then the implementation. This gives the agent a concrete verification loop: run the test, see it fail, write code, run the test, see it pass. Agents excel at this because:

- Clear success criteria (test passes)
- Built-in feedback loop (test output)
- Small iteration cycles (write test, make it pass, next test)
- Less ambiguity than "implement this feature"

**Small, atomic changes.** One concern per commit. One logical change per task. This keeps context windows small and diffs reviewable. A task like "add user authentication" becomes:

- Write auth middleware test
- Implement auth middleware
- Write login endpoint test
- Implement login endpoint
- Write token refresh test
- Implement token refresh
- Integration test

Each is a single-session task, most at Sonnet or Haiku tier.

**Explicit conventions file.** Every project gets a conventions file (generated on project add, refined over time via memory) covering:

- Error handling pattern (thiserror vs anyhow, Result types)
- Project structure conventions
- Naming conventions
- Test patterns (unit test location, integration test structure, fixtures)
- Dependency policy (which crates/packages are approved)
- Git conventions (branch naming, commit message format)

**Verify before completing.** Every worker agent runs verification before marking a task done:

1. `cargo check` / `npm run build` (does it compile?)
2. Run the tests it wrote (do they pass?)
3. Run the full test suite (did it break anything?)
4. `cargo clippy` / linter (is it clean?)

If any step fails, the agent fixes it before completing. This is enforced by the worker's system prompt, not by the orchestrator.

**Minimal diff policy.** Agents are instructed to change only what's necessary. No drive-by refactors, no reformatting unrelated code, no "while I'm here" changes. This keeps PRs reviewable and reduces merge conflicts when multiple workers touch the same project.

### Default Agent Prompts

The orchestrator ships with role-specific prompt templates:

**Worker preamble (injected into every worker session):**

```
You are a focused coding agent working on a single task.

APPROACH:
1. Read the task description and acceptance criteria
2. Understand the existing code relevant to your task
3. Write a failing test for the expected behavior
4. Implement the minimum code to make the test pass
5. Run the full test suite to verify no regressions
6. Clean up (lint, format, review your own diff)
7. Commit with a clear message and mark the task done

RULES:
- Only modify files relevant to your task
- Follow the project conventions file
- If something is ambiguous, check memory/context first, then ask for clarification
- If you're stuck for more than 2 attempts, escalate rather than thrash
- Keep your changes minimal and focused
```

**Coordinator preamble:**

```
You are the coordinator for a multi-agent coding workspace.

Your job is to plan work, decompose it into small tasks, assign them 
to workers, and track progress.

PLANNING:
- Break work into small, focused, independently testable tasks
- Identify dependencies between tasks (what must complete first?)
- Assign complexity tiers: light (haiku), standard (sonnet), heavy (opus)
- Default to standard tier. Use heavy only for architectural decisions
  or genuinely ambiguous problems. Use light for mechanical tasks.
- Prefer more small tasks over fewer large tasks

TASK QUALITY:
- Every task needs a clear title, description, and acceptance criteria
- Acceptance criteria should be verifiable (a test that passes, a 
  behavior that works, a file that exists)
- Include relevant context: what files to look at, what conventions 
  to follow, what upstream tasks produced

MONITORING:
- Check task group progress regularly
- If a worker is stuck, understand why before nudging
- Escalate to the user when genuinely blocked (missing requirements,
  conflicting constraints, access issues)
```

These are defaults. Users override via project-level config or global preferences.

## Open Questions

1. **Coordinator tool interface.** How does the coordinator agent actually "call" orchestrator commands? Options: (a) it outputs shell commands like `task create ...` that we parse (Gastown approach), (b) we expose MCP tools through the ACP session, (c) we intercept ACP tool calls. Option (a) is simplest and proven.

2. **Max concurrency defaults.** The limits config is a starting point. Need to test what Claude Max actually sustains before settling on defaults.

3. **Template discovery.** Per-project (`.templates/`), global (`~/.config/enki/templates/`), or both with override? Leaning both.

4. **Convention file generation.** When a user adds a project, auto-generate the conventions file by having an agent analyze the codebase? Light tier task, high value.

5. **Remote push strategy.** Push to remote after every merge, or batch? Gastown pushes after each. Simpler, but more network traffic. Keep Gastown's approach for v1.

6. **Memory deduplication threshold.** How similar does a new memory need to be to an existing one before we update instead of insert? FTS5 rank threshold needs tuning.
