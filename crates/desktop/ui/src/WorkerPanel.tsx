import { For, Show } from "solid-js";
import { state } from "./store";
import TierBadge from "./TierBadge";

function WorkerCard(props: {
  taskId: string;
  title: string;
  tier: string;
  activity: string;
  failed?: boolean;
}) {
  return (
    <div
      class="rounded-lg p-3 text-sm transition-colors"
      classList={{
        "bg-red-950/50 border border-red-800/50": props.failed,
        "bg-zinc-800/80": !props.failed,
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
        <span class="font-medium text-zinc-200 truncate flex-1">
          {props.title}
        </span>
        <TierBadge tier={props.tier} />
      </div>
      <div
        class="text-xs truncate pl-4"
        classList={{
          "text-red-400": props.failed,
          "text-zinc-400": !props.failed,
        }}
      >
        {props.activity}
      </div>
    </div>
  );
}

export default function WorkerPanel() {
  return (
    <div>
      <h2 class="text-xs font-medium text-zinc-500 uppercase tracking-wide mb-2 flex items-center justify-between">
        <span>Workers</span>
        <Show when={state.workerCount > 0}>
          <span class="text-zinc-400 normal-case tracking-normal font-normal">
            {state.workerCount} active
          </span>
        </Show>
      </h2>
      <div class="space-y-1.5">
        <Show
          when={state.workers.length > 0}
          fallback={
            <div class="rounded-lg bg-zinc-800/40 p-3 text-xs text-zinc-500">
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
                failed={worker.failed}
              />
            )}
          </For>
        </Show>
      </div>
    </div>
  );
}
