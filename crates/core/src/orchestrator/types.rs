use std::time::Instant;

use crate::refinery::MergeOutcome;
use crate::types::*;

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// A step definition for creating a multi-step execution.
#[derive(Debug, Clone)]
pub struct StepDef {
    pub id: String,
    pub title: String,
    pub description: String,
    pub tier: Tier,
    pub needs: Vec<String>,
}

/// Result reported by the CLI layer after a worker finishes.
#[derive(Debug)]
pub struct WorkerResult {
    pub task_id: Id,
    pub execution_id: Option<Id>,
    pub step_id: Option<String>,
    pub title: String,
    pub branch: String,
    pub outcome: WorkerOutcome,
}

#[derive(Debug)]
pub enum WorkerOutcome {
    /// Worker succeeded and has committed changes.
    Success { output: Option<String> },
    /// Worker completed but produced no changes on branch.
    NoChanges,
    /// Worker errored out.
    Failed { error: String },
}

/// Result reported by the CLI layer after the refinery finishes a merge.
#[derive(Debug)]
pub struct MergeResult {
    pub mr_id: Id,
    pub outcome: MergeOutcome,
}

/// What to pause/resume/cancel.
#[derive(Debug, Clone)]
pub enum Target {
    Execution(String),
    Node { execution_id: String, step_id: String },
}

/// Commands the CLI layer sends to the orchestrator.
#[derive(Debug)]
pub enum Command {
    /// Create a multi-step execution from step definitions.
    CreateExecution { steps: Vec<StepDef> },
    /// Create a standalone single-step task.
    CreateTask {
        title: String,
        description: Option<String>,
        tier: Tier,
    },
    /// A worker has finished (success, no-changes, or failure).
    WorkerDone(WorkerResult),
    /// The refinery has finished processing a merge request.
    MergeDone(MergeResult),
    /// Retry a failed task within its execution.
    RetryTask { task_id: Id },
    /// Pause a target.
    Pause(Target),
    /// Resume a paused target.
    Resume(Target),
    /// Cancel a target.
    Cancel(Target),
    /// Stop all running workers immediately.
    StopAll,
    /// Periodic health check on active workers.
    MonitorTick {
        workers: Vec<(String, String, Instant)>,
    },
    /// Discover new executions/tasks in DB created by external processes (MCP).
    DiscoverFromDb,
    /// Check for signal files in the events directory.
    CheckSignals,
}

// ---------------------------------------------------------------------------
// Output events
// ---------------------------------------------------------------------------

/// Events the orchestrator produces for the CLI layer to execute.
#[derive(Debug, Clone)]
pub enum Event {
    /// Spawn a worker agent for a task.
    SpawnWorker {
        task_id: Id,
        title: String,
        description: String,
        tier: Tier,
        execution_id: Id,
        step_id: String,
        upstream_outputs: Vec<(String, String)>,
    },
    /// Kill an ACP session.
    KillSession { session_id: String },
    /// Queue a merge request for the refinery.
    QueueMerge(MergeRequest),
    /// A worker completed its task successfully.
    WorkerCompleted { task_id: String, title: String },
    /// A worker failed.
    WorkerFailed {
        task_id: String,
        title: String,
        error: String,
    },
    /// A merge was successfully landed.
    MergeLanded { mr_id: String, task_id: String, branch: String },
    /// A merge had a conflict.
    MergeConflicted { mr_id: String, task_id: String, branch: String },
    /// A merge failed verification or rebasing.
    MergeFailed {
        mr_id: String,
        task_id: String,
        branch: String,
        reason: String,
    },
    /// An execution completed (all steps done).
    ExecutionComplete { execution_id: String },
    /// An execution failed (has failures, nothing left to run).
    ExecutionFailed { execution_id: String },
    /// All workers were stopped.
    AllStopped { count: usize },
    /// Monitor detected a stale worker — cancel it.
    MonitorCancel {
        session_id: String,
        task_id: String,
        stale_secs: u64,
    },
    /// Monitor escalation message.
    MonitorEscalation(String),
    /// A task is being retried after failure.
    TaskRetrying {
        task_id: String,
        title: String,
        attempt: u32,
        max: u32,
    },
    /// Informational status message for the coordinator agent.
    StatusMessage(String),
    /// A worker reported its current activity.
    WorkerReport { task_id: String, status: String },
    /// A mail message was sent (for TUI visibility).
    Mail {
        message_id: String,
        from: String,
        to: String,
        subject: String,
        priority: String,
    },
}

/// Parse a pause/cancel signal file into a Target.
pub(super) fn parse_signal_target(signal: &serde_json::Value) -> Option<Target> {
    let execution_id = signal.get("execution_id")?.as_str()?;
    if let Some(step_id) = signal.get("step_id").and_then(|v| v.as_str()) {
        Some(Target::Node {
            execution_id: execution_id.to_string(),
            step_id: step_id.to_string(),
        })
    } else {
        Some(Target::Execution(execution_id.to_string()))
    }
}
