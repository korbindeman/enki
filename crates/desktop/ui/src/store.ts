import { createStore, produce } from "solid-js/store";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import type { CoordinatorEvent, Message, Worker } from "./types";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

export interface AppState {
  /** Whether the coordinator is initialized and ready. */
  ready: boolean;
  /** Chat messages in the main panel. */
  messages: Message[];
  /** Active workers shown in the sidebar. */
  workers: Worker[];
  /** Total worker count (includes workers not yet in the list). */
  workerCount: number;
  /** Coordinator-level error, if any. */
  error: string | null;
}

const [state, setState] = createStore<AppState>({
  ready: false,
  messages: [],
  workers: [],
  workerCount: 0,
  error: null,
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
        case "tool_call_done":
          // UI can show a thinking/tool indicator; for now these are no-ops.
          break;

        case "worker_spawned":
          s.workers.push({
            taskId: event.task_id,
            title: event.title,
            tier: event.tier,
            activity: "Starting",
          });
          break;

        case "worker_completed":
          s.workers = s.workers.filter((w) => w.taskId !== event.task_id);
          break;

        case "worker_failed":
          s.workers = s.workers.filter((w) => w.taskId !== event.task_id);
          break;

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

        // Merge events, mail — displayed as system messages.
        case "merge_queued":
        case "merge_landed":
        case "merge_failed":
        case "merge_conflicted":
        case "merge_progress":
        case "mail":
          // These can be surfaced in a future events panel.
          break;
      }
    }),
  );
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/** Start listening to coordinator events. Call once on app mount. */
export function initStore(): void {
  listen<CoordinatorEvent>("coordinator", (event) => {
    handleEvent(event.payload);
  });
}
