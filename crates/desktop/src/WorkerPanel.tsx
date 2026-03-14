import { createSignal, For, Show, onCleanup } from "solid-js";
import { state, stopWorker, stopAll } from "./store";
import TierBadge from "./TierBadge";

function formatElapsed(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m ${seconds}s`;
}

function humanizeRole(role: string): string {
  return role
    .split("_")
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(" ");
}

function WorkerCard(props: {
  taskId: string;
  title: string;
  tier: string;
  activity: string;
  role?: string;
  branch?: string;
  description?: string;
  spawnedAt: number;
  failed?: boolean;
}) {
  const [expanded, setExpanded] = createSignal(false);
  const [elapsed, setElapsed] = createSignal(Date.now() - props.spawnedAt);

  const timer = setInterval(() => {
    setElapsed(Date.now() - props.spawnedAt);
  }, 1000);
  onCleanup(() => clearInterval(timer));

  return (
    <div
      class="group rounded-lg p-2.5 text-sm transition-colors cursor-pointer"
      classList={{
        "bg-red-950/50 border border-red-800/50": props.failed,
        "bg-surface/50": !props.failed,
      }}
      onClick={(e) => {
        // Don't toggle if clicking the stop button.
        if ((e.target as HTMLElement).closest("button")) return;
        setExpanded(!expanded());
      }}
    >
      <div class="flex items-center gap-2 mb-1">
        <Show
          when={!props.failed}
          fallback={
            <span class="inline-block w-2 h-2 rounded-full bg-red-500" />
          }
        >
          <span class="inline-block w-2 h-2 rounded-full bg-emerald-400 animate-pulse" />
        </Show>
        <span class="font-medium text-text truncate flex-1">
          {props.title}
        </span>
        <TierBadge tier={props.tier} />
        <Show when={!props.failed}>
          <button
            class="opacity-0 group-hover:opacity-60 hover:!opacity-100 text-text-muted hover:text-red-400 transition-opacity text-xs leading-none px-0.5"
            title="Stop worker"
            onClick={() => stopWorker(props.taskId)}
          >
            ×
          </button>
        </Show>
      </div>
      <div
        class="text-xs truncate pl-4"
        classList={{
          "text-red-400": props.failed,
          "text-text-muted": !props.failed,
        }}
      >
        {props.activity}
      </div>

      <Show when={expanded()}>
        <div class="mt-2 pt-2 border-t border-border/50 space-y-1">
          <Show when={props.role}>
            <div class="flex justify-between text-xs">
              <span class="text-text-faint">Role</span>
              <span class="text-text-muted">{humanizeRole(props.role!)}</span>
            </div>
          </Show>
          <Show when={props.branch}>
            <div class="flex justify-between text-xs">
              <span class="text-text-faint">Branch</span>
              <span class="text-text-muted font-mono">{props.branch}</span>
            </div>
          </Show>
          <div class="flex justify-between text-xs">
            <span class="text-text-faint">Elapsed</span>
            <span class="text-text-muted">{formatElapsed(elapsed())}</span>
          </div>
          <Show when={props.description}>
            <p class="text-xs text-text-faint mt-1.5 line-clamp-3">
              {props.description}
            </p>
          </Show>
        </div>
      </Show>
    </div>
  );
}

export default function WorkerPanel() {
  return (
    <div>
      <h2 class="text-[11px] font-medium text-text-muted tracking-wide mb-2 flex items-center justify-between">
        <span>Workers</span>
        <Show when={state.workerCount > 0}>
          <span class="text-text-muted normal-case tracking-normal font-normal flex items-center gap-2">
            {state.workerCount} active
            <button
              class="text-text-faint hover:text-red-400 transition-colors"
              title="Stop all workers"
              onClick={() => stopAll()}
            >
              Stop all
            </button>
          </span>
        </Show>
      </h2>
      <div class="space-y-1.5">
        <Show
          when={state.workers.length > 0}
          fallback={
            <div class="text-xs text-text-faint px-1">
              No active workers
            </div>
          }
        >
          <For each={state.workers}>
            {(worker) => (
              <WorkerCard
                taskId={worker.taskId}
                title={worker.title}
                tier={worker.tier}
                activity={worker.activity}
                role={worker.role}
                branch={worker.branch}
                description={worker.description}
                spawnedAt={worker.spawnedAt}
                failed={worker.failed}
              />
            )}
          </For>
        </Show>
      </div>
    </div>
  );
}
