# Coordinator Flow: Design Philosophy

## Core Principle

The user operates at a high level. They describe what they want, make architectural decisions, and approve direction. The Coordinator handles everything else — decomposing work, delegating to workers, tracking progress, and surfacing only what matters back to the user.

## Interaction Model

```
User (high-level requests, decisions, preferences)
  ↕
Coordinator (planning, delegation, status, clarification routing)
  ↕
Workers (research, implementation, testing)
```

The user never talks to workers directly. The Coordinator is the single interface.

## Flow

### 1. User Request

User gives a high-level request. Examples:
- "Add authentication to the API"
- "The checkout flow is broken for guest users"
- "Refactor the data layer to use the repository pattern"

### 2. Coordinator Planning

The Coordinator analyzes the request:
- **Well-defined?** → Plan and execute immediately. Tell the user what's happening.
- **Ambiguous?** → Surface the specific decisions that matter to the user. Don't ask about implementation details the user doesn't care about — ask about architectural direction, scope boundaries, and tradeoffs.

Good clarifying questions:
- "Should auth use JWT or session-based tokens? JWT is simpler but sessions give you revocation."
- "This touches the payment module — should I keep it scoped to checkout, or fix the underlying payment abstraction?"
- "There are two approaches: migrate in-place (faster, riskier) or build alongside and swap (slower, safer). Which do you prefer?"

Bad clarifying questions (don't ask these):
- "What should I name the auth middleware file?"
- "Should I use a HashMap or BTreeMap for the cache?"
- "Do you want me to add error handling?"

### 3. Execution

Once the plan is clear (either immediately or after clarification), the Coordinator creates the execution DAG and workers begin. The Coordinator provides status updates at meaningful milestones — not every tool call, but completion of steps, merge results, and any blockers.

### 4. Worker Clarification

Workers doing complex work may hit genuine ambiguity — not implementation details, but design questions that affect the user's codebase in ways they should decide. When this happens:

1. Worker sends a clarification request to the Coordinator
2. Coordinator evaluates: is this something I can answer from context, or does the user need to decide?
3. If the user needs to decide, Coordinator surfaces it with context: what the worker is doing, what the question is, what the options are
4. User answers
5. Coordinator relays the answer back to the worker

Workers should continue working on whatever they can while waiting for answers. They don't block entirely — they skip the ambiguous part and come back to it.

### 5. Completion

Coordinator reports results: what was done, what was merged, any issues encountered. Concise, not verbose.

## Progressive Autonomy

Over time, the system learns the user's preferences and decision patterns:

1. **Session memory**: Decisions made in a session inform later questions in the same session ("You chose JWT for auth — applying the same pattern to the WebSocket auth")
2. **Project memory**: Stored in `.enki/preferences/` or similar. Architectural decisions, coding conventions the user has expressed, tradeoff preferences.
3. **Implied answers**: When the system has high confidence it knows what the user would choose (based on past decisions), it acts and briefly notes what it decided and why. The user can override.

This creates a ramp: early sessions are more interactive, later sessions are more autonomous. The Coordinator grows from "assistant that asks" to "partner that acts with judgment."

## What This Changes From Today

### Currently
- Coordinator plans and executes, but doesn't have a structured clarification gate
- Workers can mail the coordinator, but there's no "ask the user through the coordinator" pattern
- No mechanism to store and recall user preferences across sessions
- Status updates are poll-based (TUI reads worker state) rather than coordinator-driven narratives
- The coordinator prompt says "ask clarifying questions" but doesn't enforce it or give it structure

### Target
- Coordinator has explicit planning → clarification → execution phases
- Workers can request user decisions through a structured escalation path
- User preferences accumulate in memory, reducing future clarification needs
- Coordinator provides narrative status updates ("Step 2/4 merged — auth middleware is in place, starting on the route handlers")
- The system gets smarter over time, not just per-session but across sessions
