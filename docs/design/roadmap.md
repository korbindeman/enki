# Roadmap: Coordinator Flow Alignment

Ordered by dependency and impact. Each phase is independently shippable.

---

## Phase 1: Structured Clarification Gate

**Goal**: The Coordinator explicitly gates on user approval before executing when the request is ambiguous. Well-defined requests skip the gate.

### Changes

1. **Coordinator prompt update** (`prompts.rs: build_system_prompt`)
   - Add explicit instruction: before calling `enki_execution_create`, present the plan to the user and wait for confirmation when:
     - Multiple valid architectural approaches exist
     - Scope is ambiguous (could be interpreted narrowly or broadly)
     - The request touches shared/critical infrastructure
   - Skip the gate when the request is clearly scoped and has one obvious approach
   - Teach the coordinator what "good" vs "bad" clarifying questions look like (architectural decisions vs implementation details)

2. **No code changes required** — this is a prompt engineering change. The planner agent already has a conversational turn with the user before calling tools. We just need to make the "plan → confirm → execute" pattern explicit in the prompt.

### Validation
- Test with ambiguous requests ("add auth") — coordinator should ask about approach before creating execution
- Test with clear requests ("fix the typo in README.md") — coordinator should just do it

---

## Phase 2: Worker → User Escalation Path

**Goal**: Workers can surface design questions to the user through the Coordinator, without blocking entirely.

### Changes

1. **New MCP tool: `enki_ask_coordinator`** (worker-facing)
   - Parameters: `question` (string), `context` (string), `options` (optional array of choices)
   - Worker calls this when it hits a genuine design decision the user should make
   - Tool writes a signal file (like existing IPC) with the question payload

2. **Coordinator handles escalation events**
   - On poll tick, coordinator picks up `AskUser` signal files
   - Presents the question to the user with worker context: "Worker on [task title] is asking: [question]"
   - Routes user's answer back to the worker via the mail system (or a new dedicated response channel)

3. **Worker continues while waiting**
   - Worker prompt instructs: after calling `enki_ask_coordinator`, continue with other parts of the task. Come back to the blocked decision when you receive the answer.
   - If the question is truly blocking (can't proceed without it), worker reports "waiting for user input" via `enki_worker_report`

4. **Coordinator filters trivial questions**
   - Not every worker question should reach the user. Coordinator prompt should instruct: if you can answer from context or prior decisions, answer directly and relay to the worker without bothering the user.

### Validation
- Worker implementing auth hits "JWT vs sessions?" → surfaces to user → user answers → worker continues
- Worker asks trivial question ("should I name the file auth.rs or authentication.rs?") → coordinator answers without escalating

---

## Phase 3: Narrative Status Updates

**Goal**: User sees meaningful progress narratives, not raw task state changes.

### Changes

1. **Coordinator-driven status messages**
   - Instead of TUI polling worker state and showing "Worker: task-title - Thinking", the Coordinator synthesizes status at meaningful milestones
   - When a step completes and merges: "Auth middleware merged — routes are next"
   - When a step fails: "Test step failed — build errors in auth.rs. Retrying with additional context."
   - When execution completes: "All 4 steps done. Auth system is in place with JWT tokens, middleware, and tests."

2. **New `FromCoordinator` message variant**: `StatusUpdate(String)`
   - TUI renders these in the chat as coordinator messages
   - Distinct from `Done` (which is the planner's full response to a user prompt)

3. **Coordinator prompt update**: instruct the planner to emit natural-language status updates between tool calls when steps complete, fail, or hit notable milestones.

### Validation
- Run a 3-step execution, verify user sees step-by-step narrative updates
- Verify updates are concise and meaningful, not noisy

---

## Phase 4: Preference Memory

**Goal**: User decisions persist across sessions, reducing future clarification needs.

### Changes

1. **Preference storage**: `.enki/preferences.toml` (project-level)
   - Key-value pairs: `auth_approach = "jwt"`, `testing_style = "integration-heavy"`, `error_handling = "explicit Result types"`
   - Structured categories: architecture, conventions, tradeoffs

2. **New MCP tool: `enki_remember_preference`** (coordinator-facing)
   - When the user makes a decision during clarification, coordinator stores it
   - Parameters: `category`, `key`, `value`, `context` (why this was decided)

3. **Preference injection into coordinator prompt**
   - On startup, load preferences and include in system prompt: "User preferences for this project: ..."
   - Coordinator uses these to answer without asking: "Based on your preference for JWT auth, I'm using the same pattern here."

4. **Preference loading into worker prompts**
   - Relevant preferences injected as context into worker prompts
   - Workers follow established preferences without needing to ask

5. **Override mechanism**
   - User can always override: "Actually, use sessions for this one"
   - Coordinator updates preference if the user indicates a permanent change

### Validation
- User says "use JWT" in session 1 → session 2 auth task uses JWT without asking
- User says "actually switch to sessions" → preference updates

---

## Phase 5: Implied Answers & Growing Autonomy

**Goal**: System acts with increasing confidence, noting decisions it made rather than asking.

### Changes

1. **Confidence-based decision making**
   - Coordinator prompt updated: when preference memory + codebase patterns give high confidence on a decision, act and note it: "I chose X because [reason]. Let me know if you'd prefer something different."
   - Low confidence → still ask

2. **Decision logging**
   - All decisions (asked or implied) logged to `.enki/decisions.log` or similar
   - User can review: "What decisions did you make on this execution?"
   - New MCP tool: `enki_list_decisions(execution_id?)`

3. **Feedback loop**
   - If user overrides an implied decision, reduce confidence for that pattern
   - If user confirms or doesn't object, increase confidence
   - This is simple heuristic-based, not ML — just counting agreements/overrides per preference key

### Validation
- After 3+ sessions with consistent choices, coordinator stops asking about those choices
- User override correctly reduces confidence and resumes asking

---

## Implementation Order & Dependencies

```
Phase 1 (prompt change only, no code)
  ↓
Phase 2 (new tool + signal file + coordinator handling)
  ↓
Phase 3 (new message variant + coordinator prompt)
  ↓
Phase 4 (preference storage + tools + prompt injection)
  ↓
Phase 5 (confidence logic + decision log)
```

Phases 1 and 3 are relatively independent and could be done in parallel. Phase 2 is the most significant code change. Phases 4 and 5 build on all prior phases.

---

## What We're NOT Changing

- The DAG scheduler, merge refinery, copy manager, and core orchestrator remain as-is
- The role system stays the same
- Worker isolation model (copy-per-task) stays the same
- Signal file IPC pattern stays the same (we're extending it, not replacing it)
- The `!Send` boundary and runtime threading model stay the same
