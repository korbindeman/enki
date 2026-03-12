import { createSignal } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import type { ContentBlock, CoordinatorEvent, Message, Task, Worker } from "./types";

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
  /** Current git branch name. */
  currentBranch: string | null;
  /** Git working tree status counts. */
  gitStatus: { modified: number; untracked: number; staged: number } | null;
}

const [state, setState] = createStore<AppState>({
  projectCwd: null,
  ready: false,
  messages: [],
  workers: [],
  workerCount: 0,
  tasks: [],
  error: null,
  currentBranch: null,
  gitStatus: null,
});

export { state };

// ---------------------------------------------------------------------------
// Backlog
// ---------------------------------------------------------------------------

export interface BacklogItem {
  id: string;
  body: string;
  created_at: string;
  updated_at: string;
}

const [backlogItems, setBacklogItems] = createSignal<BacklogItem[]>([]);
export { backlogItems };

export async function loadBacklog(): Promise<void> {
  try {
    const items = await invoke<BacklogItem[]>("backlog_list");
    setBacklogItems(items);
  } catch {
    // DB not available yet (no project open).
  }
}

export async function addBacklogItem(body: string): Promise<void> {
  await invoke("backlog_add", { body });
  await loadBacklog();
}

export async function updateBacklogItem(id: string, body: string): Promise<void> {
  await invoke("backlog_update", { id, body });
  await loadBacklog();
}

export async function removeBacklogItem(id: string): Promise<void> {
  await invoke("backlog_remove", { id });
  await loadBacklog();
}

// ---------------------------------------------------------------------------
// Actions (frontend → Rust)
// ---------------------------------------------------------------------------

let messageCounter = 0;

function nextId(): string {
  return `msg-${++messageCounter}`;
}

export async function sendPrompt(
  text: string,
  images?: Array<{ data: string; mime_type: string }>,
): Promise<void> {
  // Append user message immediately.
  setState(
    produce((s) => {
      s.messages.push({
        id: nextId(),
        role: "user",
        blocks: [{ type: "text", content: text }],
        streaming: false,
        images: images?.map((i) => `data:${i.mime_type};base64,${i.data}`),
      });
    }),
  );
  await invoke("send_prompt", { text, images: images ?? null });
}

export async function interruptCoordinator(): Promise<void> {
  await invoke("interrupt");
}

export async function stopAll(): Promise<void> {
  await invoke("stop_all");
}

export async function stopWorker(taskId: string): Promise<void> {
  await invoke("stop_worker", { taskId });
}

export async function fetchBranch(): Promise<void> {
  try {
    const branch = await invoke<string>("get_current_branch");
    setState("currentBranch", branch);
  } catch {
    setState("currentBranch", null);
  }
}

export async function fetchGitStatus(): Promise<void> {
  try {
    const status = await invoke<{ modified: number; untracked: number; staged: number }>("get_git_status");
    setState("gitStatus", status);
  } catch {
    setState("gitStatus", null);
  }
}

export async function switchAgent(agent: string): Promise<void> {
  setState({
    ready: false,
    messages: [],
    workers: [],
    workerCount: 0,
    tasks: [],
    error: null,
  });
  await invoke("set_agent", { agent });
}

export async function openProject(path: string): Promise<void> {
  // Reset all state before switching.
  setState({
    ready: false,
    messages: [],
    workers: [],
    workerCount: 0,
    tasks: [],
    error: null,
    currentBranch: null,
    gitStatus: null,
    projectCwd: path,
  });
  await invoke("open_project", { path });
  fetchBranch();
  fetchGitStatus();
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
            blocks: [{ type: "text", content: "Coordinator initializing..." }],
            streaming: false,
          });
          break;

        case "ready":
          s.ready = true;
          // Update the init message in-place if it's the last system message.
          {
            const last = s.messages.findLast((m) => m.role === "system");
            const tb = last?.blocks[0];
            if (tb && tb.type === "text" && tb.content === "Coordinator initializing...") {
              tb.content = "Coordinator ready.";
            }
          }
          break;

        case "text": {
          const last = s.messages.at(-1);
          if (last && last.role === "assistant" && last.streaming) {
            const lastBlock = last.blocks.at(-1);
            if (lastBlock && lastBlock.type === "text") {
              lastBlock.content += event.content;
            } else {
              last.blocks.push({ type: "text", content: event.content });
            }
          } else {
            s.messages.push({
              id: nextId(),
              role: "assistant",
              blocks: [{ type: "text", content: event.content }],
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

        case "tool_call": {
          const last = s.messages.at(-1);
          if (last && last.role === "assistant" && last.streaming) {
            const lastBlock = last.blocks.at(-1);
            if (lastBlock && lastBlock.type === "tools") {
              lastBlock.calls.push({ name: event.name, done: false });
            } else {
              last.blocks.push({ type: "tools", calls: [{ name: event.name, done: false }] });
            }
          } else {
            s.messages.push({
              id: nextId(),
              role: "assistant",
              blocks: [{ type: "tools", calls: [{ name: event.name, done: false }] }],
              streaming: true,
            });
          }
          break;
        }

        case "tool_call_done": {
          const lastAsst = s.messages.findLast((m) => m.role === "assistant");
          if (lastAsst) {
            for (let i = lastAsst.blocks.length - 1; i >= 0; i--) {
              const block = lastAsst.blocks[i];
              if (block.type === "tools") {
                const pending = block.calls.find((tc) => !tc.done);
                if (pending) { pending.done = true; break; }
              }
            }
          }
          break;
        }

        case "worker_spawned":
          // No chat card on spawn — card appears on completion instead.
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
          // Create card at current chat position (bottom) on completion.
          const tier = s.tasks.find((t) => t.taskId === event.task_id)?.tier ?? "standard";
          s.messages.push({
            id: nextId(),
            role: "system",
            blocks: [],
            streaming: false,
            workerCard: {
              taskId: event.task_id,
              title: event.title,
              tier,
              status: "done",
            },
          });
          // Mark worker as done, remove from sidebar.
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
          // Create failed card at current chat position.
          const failTier = s.tasks.find((t) => t.taskId === event.task_id)?.tier ?? "standard";
          s.messages.push({
            id: nextId(),
            role: "system",
            blocks: [],
            streaming: false,
            workerCard: {
              taskId: event.task_id,
              title: event.title,
              tier: failTier,
              status: "failed",
              error: event.error,
            },
          });
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
            blocks: [{ type: "text", content: `Error: ${event.message}` }],
            streaming: false,
          });
          break;

        case "merger_spawned": {
          const existing = s.workers.find(w => w.taskId === event.task_id);
          if (!existing) {
            s.workers.push({
              taskId: event.task_id,
              title: event.title,
              tier: "light",
              activity: `Resolving conflicts in ${event.conflict_files.length} file(s)`,
            });
          }
          break;
        }

        case "merge_queued": {
          const cardMsg = s.messages.find(
            (m) => m.workerCard?.taskId === event.task_id,
          );
          if (cardMsg?.workerCard) {
            cardMsg.workerCard.status = "merging";
          }
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) task.mergeStatus = "queued";
          break;
        }

        case "merge_landed": {
          const cardMsg = s.messages.find(
            (m) => m.workerCard?.taskId === event.task_id,
          );
          if (cardMsg?.workerCard) {
            cardMsg.workerCard.status = "merged";
          }
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) {
            task.mergeStatus = "landed";
            task.mergeFlashUntil = Date.now() + 2000;
          }
          // Remove merger worker from sidebar.
          s.workers = s.workers.filter(w => w.taskId !== event.task_id);
          // Branch/status may have changed after merge.
          fetchBranch();
          fetchGitStatus();
          break;
        }

        case "merge_failed": {
          const cardMsg = s.messages.find(
            (m) => m.workerCard?.taskId === event.task_id,
          );
          if (cardMsg?.workerCard) {
            cardMsg.workerCard.status = "failed";
            cardMsg.workerCard.error = event.reason;
          }
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) {
            task.mergeStatus = "failed";
            task.error = event.reason;
          }
          // Remove merger worker from sidebar.
          s.workers = s.workers.filter(w => w.taskId !== event.task_id);
          break;
        }

        case "merge_conflicted": {
          const cardMsg = s.messages.find(
            (m) => m.workerCard?.taskId === event.task_id,
          );
          if (cardMsg?.workerCard) {
            cardMsg.workerCard.status = "conflicted";
          }
          const task = s.tasks.find(
            (t) => t.taskId === event.task_id,
          );
          if (task) task.mergeStatus = "conflicted";
          break;
        }

        case "merge_progress": {
          const cardMsg = s.messages.find(
            (m) => m.workerCard?.taskId === event.task_id,
          );
          if (cardMsg?.workerCard) {
            cardMsg.workerCard.status = "merging";
          }
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
  if (cwd) {
    setState("projectCwd", cwd);
    fetchBranch();
    fetchGitStatus();
    setInterval(() => {
      fetchBranch();
      fetchGitStatus();
    }, 5000);
  }
}
