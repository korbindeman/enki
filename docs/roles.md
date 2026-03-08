# Worker Roles

Enki workers are specialized agents that execute individual steps in an execution. Each worker runs in an isolated copy of the repository, receives a task prompt, does its work, and produces output that gets merged back (or saved as an artifact).

This document describes each built-in role: what it does, what it receives, what it produces, how it works, and when to use it.

## How Workers Run

Every worker, regardless of role, follows the same lifecycle:

1. **Copy** — Enki creates a CoW copy of the project at HEAD
2. **Spawn** — An ACP agent session starts in the copy directory
3. **Prompt** — The worker receives: role system prompt + task title + task description + upstream context (if any)
4. **Execute** — The worker does its job using the agent's built-in tools (file read/write, bash, search) plus enki MCP tools
5. **Output** — The worker emits an `[OUTPUT]...[/OUTPUT]` summary visible to downstream steps
6. **Finish** — For code-change roles, enki commits the diff and merges the branch via the refinery. For artifact roles, the markdown report is saved to `.enki/artifacts/`

### MCP Tools Available to Workers

All workers have access to these enki-specific tools (in addition to the agent's built-in file/bash/search tools):

| Tool | Purpose |
|------|---------|
| `enki_worker_report` | Report current activity phase (visible in TUI status) |
| `enki_edit_file` | Hashline-verified file editing (detects stale edits) |
| `enki_status` | View task counts |
| `enki_task_list` | View all tasks |
| `enki_dag` | View execution dependency graph |
| `enki_mail_send` | Send a message to the coordinator or other workers |
| `enki_mail_check` | Check inbox |
| `enki_mail_inbox` | List all messages |
| `enki_mail_reply` | Reply to a message |
| `enki_mail_thread` | View a message thread |

Read-only roles (`researcher`, `code_referencer`) get the same set minus `enki_edit_file`.

---

## `feature_developer`

Implements new features end-to-end: production code, tests, and integration with existing patterns.

### Input

- **Task title**: Short name for the feature
- **Task description**: What to build, acceptance criteria, which files/modules are relevant
- **Upstream context** (optional): Output summaries or artifact files from prior steps (e.g., a researcher's findings)

### Output

- **Code changes** on an isolated branch, auto-merged to main
- **`[OUTPUT]` summary**: Files modified, approach taken, key decisions

### Process

1. **Explore** — Reads the codebase to find analogous features, conventions, shared utilities, and architecture docs. Reports findings.
2. **Design** — Picks a single implementation approach consistent with existing patterns. Identifies files to create/modify.
3. **Implement** — Writes production code and tests following project conventions. Reports progress at milestones.
4. **Self-review** — Re-reads all changes checking for bugs, pattern inconsistencies, missing test coverage, and accidental complexity. Fixes issues found.

### When to Use

- New feature implementation (API endpoint, UI component, CLI command, data model)
- Significant enhancements to existing features
- Any task that requires writing both implementation and tests
- Work that needs to integrate carefully with existing codebase patterns

### Example Tasks

- "Add JWT-based authentication middleware to the API server"
- "Implement CSV export for the reports module"
- "Add a `--dry-run` flag to the deploy command"

---

## `bug_fixer`

Diagnoses root causes and applies minimal, surgical fixes with regression tests.

### Input

- **Task title**: Bug summary
- **Task description**: Bug report — symptoms, reproduction steps, affected code paths, error messages
- **Upstream context** (optional): Researcher findings, stack traces, log excerpts

### Output

- **Code changes** on an isolated branch, auto-merged to main
- **`[OUTPUT]` summary**: Root cause, fix applied, regression test added

### Process

1. **Understand** — Reads the relevant code and mentally reproduces the issue
2. **Root-cause** — Traces to the fundamental problem, not just the symptom
3. **Fix** — Makes the minimal change to correct the bug
4. **Test** — Writes a regression test that would have caught this bug
5. **Verify** — Confirms the fix doesn't break adjacent functionality

### When to Use

- Known bugs with clear symptoms or reproduction steps
- Crash reports, incorrect behavior, data corruption issues
- When the goal is a targeted fix, not a refactor

### Example Tasks

- "Fix panic when parsing empty config files — see stack trace in issue #42"
- "User registration fails silently when email contains a plus sign"
- "Race condition in the connection pool causes intermittent timeouts under load"

---

## `ralph`

Iterative verify-fix loop worker. Runs a verification command, reads the failures, fixes them one at a time, and repeats until clean. Named for doing the repetitive grind work.

### Input

- **Task title**: What needs to pass
- **Task description**: The verification command to run (or enough context to figure it out), what "passing" looks like, and any relevant constraints
- **Upstream context** (optional): Output from prior steps that introduced the failures

### Output

- **Code changes** on an isolated branch, auto-merged to main
- **`[OUTPUT]` summary**: Number of iterations, what was fixed

### Process

1. **Verify** — Runs the specified command (tests, build, lint, type-check)
2. **Assess** — Reads the output, identifies the first failure
3. **Fix** — Makes the minimal change to fix that specific failure
4. **Repeat** — Goes back to step 1. Continues until verification passes clean.

Key rules: always starts by running verification (never guesses), fixes one thing at a time, doesn't refactor or improve — only makes verification pass, and steps back to re-read code if stuck on the same failure for 3+ attempts.

### When to Use

- Making tests pass after a large refactor or dependency upgrade
- Fixing build errors across many files (e.g., after an API change)
- Lint/format compliance sweeps
- Any task with a clear programmatic pass/fail criterion

### Example Tasks

- "Run `cargo test -p enki-core` and fix all failures"
- "Fix all TypeScript type errors after upgrading to v5"
- "Make `clippy` pass with zero warnings on the entire workspace"

---

## `researcher`

Investigates code, traces execution paths, and answers architectural questions. Read-only — produces a markdown artifact, no code changes.

### Input

- **Task title**: Research question or area to investigate
- **Task description**: What to investigate, what questions to answer, which areas of the codebase to focus on

### Output

- **Markdown artifact** saved to `.enki/artifacts/<execution_id>/<step_id>.md`
- **`[OUTPUT]` summary**: 2-5 sentence summary of findings, visible to downstream steps

### Process

1. Reads files, traces code paths, searches for patterns
2. Reports progress at major phases via `enki_worker_report`
3. Writes a structured markdown report with file paths, line numbers, and code snippets
4. Summarizes key findings in `[OUTPUT]` tags

### When to Use

- Understanding how a subsystem works before implementing a feature (use as an upstream dependency)
- Answering architectural questions ("how does auth flow through the middleware stack?")
- Mapping dependencies, call graphs, or data flows
- Providing context for a checkpoint decision

### Example Tasks

- "Trace the request lifecycle from HTTP handler to database query in the users module"
- "Document all places where the config is loaded and how overrides cascade"
- "Investigate why the test suite takes 3 minutes — identify the slowest tests and their bottlenecks"

---

## `code_referencer`

Fetches and references code from external sources — GitHub repositories, libraries, documentation. Read-only, produces a markdown artifact.

### Input

- **Task title**: What to look up
- **Task description**: Which repositories, libraries, or APIs to reference. What patterns, interfaces, or implementation details to extract.

### Output

- **Markdown artifact** saved to `.enki/artifacts/<execution_id>/<step_id>.md`
- **`[OUTPUT]` summary**: Key findings with source citations

### Process

1. Clones reference repositories (`git clone --depth 1`) or reads external documentation
2. Extracts relevant patterns, APIs, interfaces, and code snippets
3. Writes a structured report with repo URLs, file paths, and code excerpts
4. Cleans up cloned repos when done

### When to Use

- Studying how another project implements a similar feature
- Extracting API schemas or interface definitions from a dependency's source
- Gathering reference implementations before designing a new feature
- Looking up library internals that aren't well documented

### Example Tasks

- "Look at how `tokio` implements `JoinSet` — we want to build something similar for our task scheduler"
- "Extract the ACP protocol schema from `@anthropic-ai/agent-client-protocol` and document the session lifecycle"
- "Find how `ripgrep` handles glob patterns and report the relevant code paths"

---

## `documenter`

Adds or improves docstrings and API documentation following language-specific conventions. Only touches documentation — never modifies logic.

### Input

- **Task title**: What to document
- **Task description**: Which files, modules, or APIs to document. Any style preferences or focus areas.

### Output

- **Code changes** on an isolated branch, auto-merged to main
- **`[OUTPUT]` summary**: Files documented, conventions followed

### Process

1. **Read** — Understands the code by reading implementation, callers, and tests
2. **Survey** — Checks existing documentation style in the project to match it
3. **Document** — Adds docstrings to the public API surface following language-specific conventions:
   - **Rust**: `///` items, `//!` modules, `# Panics`/`# Errors`/`# Safety`/`# Examples` sections, doc-tests
   - **Python**: PEP 257 triple-quote docstrings (Google/NumPy/Sphinx style), `Args:`/`Returns:`/`Raises:`
   - **TypeScript/JS**: JSDoc `/** */` with `@param`, `@returns`, `@throws`, `@example`
   - **Go**: `//` comments starting with the symbol name, godoc conventions
4. **Skip the obvious** — Doesn't document trivial getters/setters or restate function names

### When to Use

- Adding docstrings to a module or crate that lacks them
- Documenting a public API before publishing
- Post-implementation documentation pass
- Onboarding aid — making a complex subsystem easier to understand

### Example Tasks

- "Add docstrings to all public types and functions in `crates/core/src/orchestrator.rs`"
- "Document the `enki_acp` crate's public API — focus on `AgentManager` and session lifecycle"
- "Add JSDoc comments to all exported functions in `src/api/routes/`"

---

## Default (no role)

When no `role` is specified, the worker gets a generic system prompt: *"You are a focused coding agent working on a single task."* It receives the same task structure and MCP tools as other code-change roles.

### When to Use

- Simple, well-scoped tasks that don't need specialized behavior
- Tasks where the description is detailed enough that role-specific guidance isn't needed
- Quick mechanical changes that don't fit neatly into another role

### Example Tasks

- "Rename the `get_config` function to `load_config` across the codebase"
- "Update the CI workflow to use Node 22"
- "Remove the deprecated `legacy_auth` module and all references to it"

---

## Choosing a Role

| Situation | Role |
|-----------|------|
| Build a new feature with tests | `feature_developer` |
| Fix a known bug | `bug_fixer` |
| Make tests/build/lint pass | `ralph` |
| Understand code before building | `researcher` |
| Study an external repo or library | `code_referencer` |
| Add docstrings to existing code | `documenter` |
| Simple mechanical edit | _(default, no role)_ |

## Custom Roles

Roles can be overridden or extended via TOML files:

- **Global**: `~/.enki/roles/*.toml`
- **Per-project**: `.enki/roles/*.toml`

Later entries override earlier ones by name. See `docs/configuration.md` for the TOML schema.
