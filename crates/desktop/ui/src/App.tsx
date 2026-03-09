import {
  createEffect,
  createSignal,
  For,
  Show,
  Switch,
  Match,
  onMount,
  onCleanup,
} from "solid-js";
import {
  state,
  initStore,
  sendPrompt,
  interruptCoordinator,
} from "./store";
import { renderMarkdown } from "./markdown";
import type { Message } from "./types";
import WorkerPanel from "./WorkerPanel";
import TaskList from "./TaskList";

function ChatMessage(props: { message: Message }) {
  return (
    <Switch>
      <Match when={props.message.role === "system"}>
        <div class="text-xs text-zinc-500 text-center py-2">
          {props.message.content}
        </div>
      </Match>
      <Match when={props.message.role === "user"}>
        <div class="py-3">
          <div class="text-xs font-medium text-zinc-500 mb-1.5">You</div>
          <div class="text-sm whitespace-pre-wrap text-zinc-100">
            {props.message.content}
          </div>
        </div>
      </Match>
      <Match when={props.message.role === "assistant"}>
        <div class="py-3">
          <div class="text-xs font-medium text-zinc-500 mb-1.5">Enki</div>
          <div
            class="prose"
            innerHTML={renderMarkdown(
              props.message.content,
              props.message.streaming,
            )}
          />
          <Show when={props.message.streaming}>
            <span class="inline-block w-1.5 h-4 bg-zinc-400 animate-pulse align-middle mt-1" />
          </Show>
        </div>
      </Match>
    </Switch>
  );
}

function App() {
  let messagesContainer!: HTMLDivElement;
  let textareaRef!: HTMLTextAreaElement;
  const [input, setInput] = createSignal("");

  const isStreaming = () => {
    const msgs = state.messages;
    const last = msgs[msgs.length - 1];
    return !!(last?.role === "assistant" && last?.streaming);
  };

  onMount(() => {
    initStore();
  });

  // Auto-scroll on new messages or streaming content updates
  createEffect(() => {
    const len = state.messages.length;
    if (len > 0) {
      // Access content to track streaming updates
      void state.messages[len - 1].content;
    }
    messagesContainer?.scrollTo({
      top: messagesContainer.scrollHeight,
      behavior: "smooth",
    });
  });

  // Global Ctrl+C to interrupt
  function handleGlobalKeydown(e: KeyboardEvent) {
    if (e.ctrlKey && e.key === "c" && isStreaming()) {
      e.preventDefault();
      interruptCoordinator();
    }
  }

  onMount(() => window.addEventListener("keydown", handleGlobalKeydown));
  onCleanup(() => window.removeEventListener("keydown", handleGlobalKeydown));

  function handleSubmit() {
    const text = input().trim();
    if (!text || isStreaming()) return;
    setInput("");
    sendPrompt(text);
    if (textareaRef) {
      textareaRef.style.height = "auto";
    }
  }

  function handleTextareaKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSubmit();
    }
  }

  function autoResize(el: HTMLTextAreaElement) {
    el.style.height = "auto";
    el.style.height = Math.min(el.scrollHeight, 200) + "px";
  }

  return (
    <div class="flex h-screen bg-zinc-900 text-zinc-100">
      {/* Sidebar */}
      <aside class="w-[300px] shrink-0 border-r border-zinc-700 bg-zinc-950 flex flex-col">
        <div class="px-4 py-3 border-b border-zinc-700">
          <h1 class="text-lg font-semibold">Enki</h1>
        </div>
        <div class="flex-1 overflow-y-auto p-3 space-y-5">
          <WorkerPanel />
          <div class="border-t border-zinc-800" />
          <TaskList />
        </div>
      </aside>

      {/* Main chat area */}
      <main class="flex-1 flex flex-col min-w-0">
        <header class="px-6 py-3 border-b border-zinc-700 flex items-center justify-between">
          <h2 class="text-lg font-semibold">Chat</h2>
          <Show when={!state.ready}>
            <span class="text-xs text-zinc-500 animate-pulse">
              Connecting...
            </span>
          </Show>
        </header>

        {/* Messages */}
        <div ref={messagesContainer} class="flex-1 overflow-y-auto">
          <div class="max-w-3xl mx-auto py-6 px-6">
            <Show when={state.messages.length === 0}>
              <div class="text-zinc-500 text-sm pt-8 text-center">
                Start a conversation to begin orchestrating...
              </div>
            </Show>
            <For each={state.messages}>
              {(msg) => <ChatMessage message={msg} />}
            </For>
          </div>
        </div>

        {/* Tool indicator + Input */}
        <div class="border-t border-zinc-700">
          <Show when={state.activeToolCall}>
            <div class="px-6 py-1.5 text-xs text-zinc-500 flex items-center gap-2">
              <span class="inline-block w-1.5 h-1.5 rounded-full bg-amber-500 animate-pulse" />
              Using {state.activeToolCall}
            </div>
          </Show>
          <Show when={isStreaming() && !state.activeToolCall}>
            <div class="px-6 py-1.5 text-xs text-zinc-500 flex items-center gap-2">
              <span class="inline-block w-1.5 h-1.5 rounded-full bg-zinc-400 animate-pulse" />
              Responding...
            </div>
          </Show>
          <div class="px-6 py-4">
            <div class="max-w-3xl mx-auto flex gap-3 items-end">
              <textarea
                ref={textareaRef}
                value={input()}
                onInput={(e) => {
                  setInput(e.currentTarget.value);
                  autoResize(e.currentTarget);
                }}
                onKeyDown={handleTextareaKeydown}
                disabled={!state.ready}
                placeholder={
                  state.ready
                    ? "Type a message..."
                    : "Waiting for coordinator..."
                }
                rows={1}
                class="flex-1 resize-none rounded-lg bg-zinc-800 border border-zinc-600 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-500 focus:outline-none focus:border-zinc-400 disabled:opacity-50 disabled:cursor-not-allowed"
              />
              <Show
                when={isStreaming()}
                fallback={
                  <button
                    onClick={handleSubmit}
                    disabled={!state.ready || !input().trim()}
                    class="rounded-lg bg-zinc-700 px-4 py-2.5 text-sm font-medium hover:bg-zinc-600 transition-colors disabled:opacity-40 disabled:cursor-not-allowed shrink-0"
                  >
                    Send
                  </button>
                }
              >
                <button
                  onClick={() => interruptCoordinator()}
                  class="rounded-lg bg-red-900/50 text-red-300 px-4 py-2.5 text-sm font-medium hover:bg-red-900/70 transition-colors shrink-0"
                  title="Interrupt (Ctrl+C)"
                >
                  Stop
                </button>
              </Show>
            </div>
          </div>
        </div>
      </main>
    </div>
  );
}

export default App;
