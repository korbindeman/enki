import { createStore, produce } from "solid-js/store";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import type { CoordinatorEvent, Message, Task, Worker } from "./types";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

export interface AppState {
  /** Project working directory. */
  projectCwd: string | null;
  /** Whether the coordinator is initialized and ready. */
  ready: boolean;
  /** Chat messages in the main panel. */
  messages: Message[];
  /** Active workers shown in the sidebar. */
  workers: Worker[];
  /** Total worker count (includes workers not yet in the list). */
  workerCount: number;
  /** Persistent task list (survives worker lifecycle). */
  tasks: Task[];
  /** Coordinator-level error, if any. */
  error: string | null;
  /** Name of the tool currently being called by the planner, if any. */
  activeToolCall: string | null;
}

const [state, setState] = createStore<AppState>({
  projectCwd: null,
  ready: false,
  messages: [],
  workers: [],
  workerCount: 0,
  tasks: [],
  error: null,
  activeToolCall: null,
});

export { state };

// ---------------------------------------------------------------------------
// Actions (frontend → Rust)
// ---------------------------------------------------------------------------

let messageCounter = 0;

function nextId(): string {
  return `msg-${++messageCounter}`;
}

export async function sendPrompt(text: string): Promise<void> {
  // Append user message immediately.
  setState(
    produce((s) => {
      s.messages.push({
        id: nextId(),
        role: "user",
        content: text,
        streaming: false,
      });
    }),
  );
  await invoke("send_prompt", { text });
}

export async function interruptCoordinator(): Promise<void> {
  await invoke("interrupt");
}

export async function stopAll(): Promise<void> {
  await invoke("stop_all");
}

// ---------------------------------------------------------------------------
// Event handling (Rust → frontend)
// ---------------------------------------------------------------------------

function handleEvent(event: CoordinatorEvent): void {
  setState(
    produce((s) => {
      switch (event.type) {
        case "connected":
          s.messages.push({
            id: nextId(),
            role: "system",
            content: "Coordinator initializing...",
            streaming: false,
          });
          break;

        case "ready":
          s.ready = true;
          // Update the init message in-place if it's the last system message.
          {
            const last = s.messages.findLast((m) => m.role === "system");
            if (last && last.content === "Coordinator initializing...") {
              last.content = "Coordinator ready.";
            }
          }
          break;

        case "text": {
          const last = s.messages.at(-1);
          if (last && last.role === "assistant" && last.streaming) {
            last.content += event.content;
          } else {
            s.messages.push({
              id: nextId(),
              role: "assistant",
              content: event.content,
              streaming: true,
            });
          }
          break;
        }

        case "done": {
          const last = s.messages.at(-1);
          if (last && last.role === "assistant" && last.streaming) {
            last.streaming = false;
          }
          break;
        }

        case "tool_call":
          s.activeToolCall = event.name;
          break;

        case "tool_call_done":
          s.activeToolCall = null;
          break;

        case "worker_spawned":
          s.workers.push({
            taskId: event.task_id,
            title: event.title,
            tier: event.tier,
            activity: "Starting",
          });
          // Add or update task entry.
          {
            const existing = s.tasks.find(
              (t) => t.taskId === event.task_id,
            );
            if (existing) {
              existing.status = "running";
              existing.error = undefined;
            } else {
              s.tasks.push({
                taskId: event.task_id,
                title: event.title,
                tier: event.tier,
                status: "running",
                mergeStatus: "none",
              });
            }
          }
          break;

        case "worker_completed": {
          // Mark worker as done, remove after brief delay.
          const cw = s.workers.find(
            (w) => w.taskId === event.task_id,
          );
          if (cw) {
            cw.activity = "Done";
          }
          s.workers = s.workers.filter(
            (w) => w.taskId !== event.task_id,
          );
          // Update task status.
          {
            const task = s.tasks.find(
              (t) => t.taskId === event.task_id,
            );
            if (task) {
              task.status = "completed";
            }
          }
          break;
        }

        case "worker_failed": {
          // Mark worker as failed briefly.
          const fw = s.workers.find(
            (w) => w.taskId === event.task_id,
          );
          if (fw) {
            fw.failed = true;
            fw.failError = event.error;
            fw.activity = "Failed";
          }
          // Schedule removal after 2s.
          const failTaskId = event.task_id;
          setTimeout(() => {
            setState(
              produce((s2) => {
                s2.workers = s2.workers.filter(
                  (w) => w.taskId !== failTaskId,
                );
              }),
            );
          }, 2000);
          // Update task status.
          {
            const task = s.tasks.find(
              (t) => t.taskId === event.task_id,
            );
            if (task) {
              task.status = "failed";
              task.error = event.error;
            }
          }
          break;
        }

        case "worker_update": {
          const worker = s.workers.find((w) => w.taskId === event.task_id);
          if (worker) {
            switch (event.activity.type) {
              case "tool_started":
                worker.activity = event.activity.name;
                break;
              case "tool_done":
              case "thinking":
                worker.activity = "Thinking";
                break;
            }
          }
          break;
        }

        case "worker_report": {
          const worker = s.workers.find((w) => w.taskId === event.task_id);
          if (worker) {
            worker.activity = event.status;
          }
          break;
        }

        case "worker_count":
          s.workerCount = event.count;
          break;

        case "all_stopped":
          s.workers = [];
          s.workerCount = 0;
          for (const task of s.tasks) {
            if (task.status === "running") {
              task.status = "failed";
            }
          }
          break;

        case "interrupted": {
          const last = s.messages.at(-1);
          if (last && last.role === "assistant" && last.streaming) {
            last.streaming = false;
          }
          break;
        }

        case "error":
          s.error = event.message;
          s.messages.push({
            id: nextId(),
            role: "system",
            content: `Error: ${event.message}`,
            streaming: false,
          });
          break;

        case "merge_queued": {
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) task.mergeStatus = "queued";
          break;
        }

        case "merge_landed": {
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) {
            task.mergeStatus = "landed";
            task.mergeFlashUntil = Date.now() + 2000;
          }
          break;
        }

        case "merge_failed": {
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) {
            task.mergeStatus = "failed";
            task.error = event.reason;
          }
          break;
        }

        case "merge_conflicted": {
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) task.mergeStatus = "conflicted";
          break;
        }

        case "merge_progress": {
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) task.mergeStatus = "merging";
          break;
        }

        case "mail":
          // Mail events can be surfaced in a future events panel.
          break;
      }
    }),
  );
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/** Start listening to coordinator events. Call once on app mount. */
export async function initStore(): Promise<void> {
  listen<CoordinatorEvent>("coordinator", (event) => {
    handleEvent(event.payload);
  });

  const cwd = await invoke<string>("get_project_dir");
  setState("projectCwd", cwd);
}
