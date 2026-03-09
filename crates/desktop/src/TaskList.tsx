import { createMemo, For, Show } from "solid-js";
import { state } from "./store";
import type { Task, TaskStatus, MergeStatus } from "./types";
import TierBadge from "./TierBadge";

const statusOrder: Record<TaskStatus, number> = {
  running: 0,
  pending: 1,
  completed: 2,
  failed: 3,
};

const statusDot: Record<TaskStatus, string> = {
  pending: "bg-zinc-500",
  running: "bg-blue-400",
  completed: "bg-emerald-400",
  failed: "bg-red-400",
};

function MergeIndicator(props: { status: MergeStatus; flash: boolean }) {
  return (
    <Show when={props.status !== "none"}>
      <span
        class="text-[10px] leading-none rounded px-1 py-0.5"
        classList={{
          "bg-zinc-700 text-zinc-400": props.status === "queued",
          "bg-blue-900/60 text-blue-300": props.status === "merging",
          "bg-emerald-900/60 text-emerald-300": props.status === "landed",
          "bg-red-900/60 text-red-300":
            props.status === "failed" || props.status === "conflicted",
          "animate-flash": props.flash,
        }}
      >
        {props.status === "queued" && "merge queued"}
        {props.status === "merging" && "merging"}
        {props.status === "landed" && "merged"}
        {props.status === "failed" && "merge failed"}
        {props.status === "conflicted" && "conflict"}
      </span>
    </Show>
  );
}

function TaskRow(props: { task: Task }) {
  const isFlashing = () => {
    const until = props.task.mergeFlashUntil;
    return until != null && Date.now() < until;
  };

  return (
    <div class="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-800/40 transition-colors">
      <span
        class={`inline-block w-2 h-2 rounded-full shrink-0 ${statusDot[props.task.status]}`}
        classList={{
          "animate-pulse": props.task.status === "running",
        }}
      />
      <span class="text-sm text-zinc-300 truncate flex-1">
        {props.task.title}
      </span>
      <MergeIndicator
        status={props.task.mergeStatus}
        flash={isFlashing()}
      />
      <TierBadge tier={props.task.tier} />
    </div>
  );
}

export default function TaskList() {
  const sorted = createMemo(() =>
    [...state.tasks].sort(
      (a, b) => statusOrder[a.status] - statusOrder[b.status],
    ),
  );

  return (
    <div>
      <h2 class="text-[11px] font-medium text-zinc-500 tracking-wide mb-2 flex items-center justify-between">
        <span>Tasks</span>
        <Show when={state.tasks.length > 0}>
          <span class="text-zinc-400 normal-case tracking-normal font-normal">
            {state.tasks.length}
          </span>
        </Show>
      </h2>
      <div class="space-y-0.5">
        <Show
          when={sorted().length > 0}
          fallback={
            <div class="text-xs text-zinc-600 px-1">
              No tasks yet
            </div>
          }
        >
          <For each={sorted()}>
            {(task) => <TaskRow task={task} />}
          </For>
        </Show>
      </div>
    </div>
  );
}
