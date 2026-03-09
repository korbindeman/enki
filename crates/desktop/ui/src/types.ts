/** Activity update from a running worker agent. */
export type WorkerActivityEvent =
  | { type: "tool_started"; name: string }
  | { type: "tool_done" }
  | { type: "thinking" };

/** Events emitted from the Rust coordinator to the frontend. */
export type CoordinatorEvent =
  | { type: "connected" }
  | { type: "ready" }
  | { type: "text"; content: string }
  | { type: "done"; content: string }
  | { type: "tool_call"; name: string }
  | { type: "tool_call_done"; name: string }
  | { type: "worker_spawned"; task_id: string; title: string; tier: string }
  | { type: "worker_completed"; task_id: string; title: string }
  | {
      type: "worker_failed";
      task_id: string;
      title: string;
      error: string;
    }
  | {
      type: "worker_update";
      task_id: string;
      activity: WorkerActivityEvent;
    }
  | { type: "worker_report"; task_id: string; status: string }
  | { type: "merge_queued"; task_id: string; branch: string }
  | { type: "merge_landed"; task_id: string; branch: string }
  | {
      type: "merge_failed";
      task_id: string;
      branch: string;
      reason: string;
    }
  | { type: "merge_conflicted"; task_id: string; branch: string }
  | {
      type: "merge_progress";
      task_id: string;
      branch: string;
      status: string;
    }
  | { type: "worker_count"; count: number }
  | { type: "all_stopped"; count: number }
  | {
      type: "mail";
      from: string;
      to: string;
      subject: string;
      priority: string;
    }
  | { type: "interrupted" }
  | { type: "error"; message: string };

/** A chat message displayed in the main panel. */
export interface Message {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  /** Set to true while the assistant is still streaming. */
  streaming: boolean;
}

/** A tracked worker shown in the sidebar. */
export interface Worker {
  taskId: string;
  title: string;
  tier: string;
  activity: string;
  /** Set briefly on failure before removal. */
  failed?: boolean;
  failError?: string;
}

/** Status of a persistent task entry. */
export type TaskStatus = "pending" | "running" | "completed" | "failed";

/** Merge state for a task. */
export type MergeStatus = "none" | "queued" | "merging" | "landed" | "failed" | "conflicted";

/** A persistent task entry shown in the task list. */
export interface Task {
  taskId: string;
  title: string;
  tier: string;
  status: TaskStatus;
  mergeStatus: MergeStatus;
  error?: string;
  /** Timestamp for flash animations (merge landed). */
  mergeFlashUntil?: number;
}
