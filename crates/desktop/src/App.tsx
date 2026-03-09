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
  openProject,
} from "./store";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { renderMarkdown } from "./markdown";
import type { Message } from "./types";
import WorkerPanel from "./WorkerPanel";
import TaskList from "./TaskList";
import Settings from "./Settings";

interface PendingImage {
  id: string;
  data: string; // base64
  mime_type: string;
  url: string; // data URL for preview
}

function ChatMessage(props: { message: Message }) {
  return (
    <Switch>
      <Match when={props.message.role === "system"}>
        <div class="text-xs text-zinc-500 text-center py-3">
          {props.message.content}
        </div>
      </Match>
      <Match when={props.message.role === "user"}>
        <div class="border-l-2 border-zinc-600 pl-4 py-3">
          <div class="text-sm whitespace-pre-wrap text-zinc-100">
            {props.message.content}
          </div>
        </div>
      </Match>
      <Match when={props.message.role === "assistant"}>
        <div class="py-3">
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
  const [pendingImages, setPendingImages] = createSignal<PendingImage[]>([]);
  const [settingsOpen, setSettingsOpen] = createSignal(false);

  const isStreaming = () => {
    const msgs = state.messages;
    const last = msgs[msgs.length - 1];
    return !!(last?.role === "assistant" && last?.streaming);
  };

  onMount(() => {
    initStore();
    listen("menu-open-project", () => handleOpenProject());
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
    if ((e.metaKey || e.ctrlKey) && e.key === ",") {
      e.preventDefault();
      setSettingsOpen((v) => !v);
    }
  }

  onMount(() => window.addEventListener("keydown", handleGlobalKeydown));
  onCleanup(() => window.removeEventListener("keydown", handleGlobalKeydown));

  function handlePaste(e: ClipboardEvent) {
    const items = e.clipboardData?.items;
    if (!items) return;
    for (const item of items) {
      if (item.type.startsWith("image/")) {
        e.preventDefault();
        const file = item.getAsFile();
        if (!file) continue;
        const mime = item.type;
        const reader = new FileReader();
        reader.onload = () => {
          const dataUrl = reader.result as string;
          const base64 = dataUrl.split(",")[1];
          setPendingImages((prev) => [
            ...prev,
            { id: crypto.randomUUID(), data: base64, mime_type: mime, url: dataUrl },
          ]);
        };
        reader.readAsDataURL(file);
      }
    }
  }

  function removeImage(id: string) {
    setPendingImages((prev) => prev.filter((i) => i.id !== id));
  }

  function handleSubmit() {
    const text = input().trim();
    const images = pendingImages();
    if ((!text && images.length === 0) || isStreaming()) return;
    setInput("");
    setPendingImages([]);
    const payload = images.length > 0
      ? images.map((i) => ({ data: i.data, mime_type: i.mime_type }))
      : undefined;
    sendPrompt(text, payload);
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

  async function handleOpenProject() {
    const folder = await open({ directory: true });
    if (folder) {
      await openProject(folder);
    }
  }

  return (
    <div class="flex h-screen bg-zinc-900 text-zinc-100">
      {/* Sidebar */}
      <aside class="w-[260px] shrink-0 border-r border-zinc-800 bg-zinc-950 flex flex-col">
        <div class="px-4 py-3">
          <div class="flex items-center justify-between">
            <h1 class="text-lg font-semibold">Enki</h1>
            <button
              onClick={() => setSettingsOpen(true)}
              class="text-zinc-500 hover:text-zinc-300 transition-colors"
              title="Settings (⌘,)"
            >
              <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
              </svg>
            </button>
          </div>
          <Show when={state.projectCwd}>
            <div class="text-xs text-zinc-500 truncate mt-0.5 flex items-center gap-1.5">
              <span title={state.projectCwd!}>{state.projectCwd!.split("/").pop()}</span>
              <Show when={state.currentBranch}>
                <span class="text-zinc-600">/</span>
                <span class="text-zinc-400">{state.currentBranch}</span>
              </Show>
            </div>
          </Show>
        </div>
        <div class="flex-1 overflow-y-auto p-4 space-y-6">
          <WorkerPanel />
          <TaskList />
        </div>
      </aside>

      {/* Main chat area */}
      <main class="flex-1 flex flex-col min-w-0">
        <Show
          when={state.projectCwd}
          fallback={
            <div class="flex-1 flex items-center justify-center">
              <div class="text-center space-y-4">
                <h2 class="text-lg font-semibold text-zinc-300">No project open</h2>
                <p class="text-sm text-zinc-500">Use File &gt; Open Project, or click below.</p>
                <button
                  onClick={handleOpenProject}
                  class="rounded-lg bg-zinc-700 px-5 py-2.5 text-sm font-medium hover:bg-zinc-600 transition-colors"
                >
                  Open Project...
                </button>
              </div>
            </div>
          }
        >
          {/* Messages */}
          <div ref={messagesContainer} class="flex-1 overflow-y-auto">
            <div class="max-w-3xl mx-auto py-8 px-6">
              <Show when={state.messages.length === 0}>
                <div class="text-zinc-500 text-sm pt-20 text-center">
                  Start a conversation to begin orchestrating...
                </div>
              </Show>
              <For each={state.messages}>
                {(msg) => <ChatMessage message={msg} />}
              </For>
            </div>
          </div>

          {/* Tool indicator + Input */}
          <div class="relative">
            <div class="absolute top-0 left-0 right-0 -translate-y-full bg-gradient-to-t from-zinc-900 to-transparent h-8 pointer-events-none" />
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
            <div class="px-6 py-3 pb-5">
              <Show when={pendingImages().length > 0}>
                <div class="max-w-3xl mx-auto pb-3 flex gap-2 flex-wrap">
                  <For each={pendingImages()}>
                    {(img) => (
                      <div class="relative group">
                        <img
                          src={img.url}
                          alt=""
                          class="h-20 rounded-lg border border-zinc-600 object-cover"
                        />
                        <button
                          onClick={() => removeImage(img.id)}
                          class="absolute -top-1.5 -right-1.5 w-5 h-5 rounded-full bg-zinc-700 border border-zinc-500 text-zinc-300 flex items-center justify-center text-xs hover:bg-red-900 hover:border-red-700 hover:text-red-300 transition-colors opacity-0 group-hover:opacity-100"
                        >
                          &times;
                        </button>
                      </div>
                    )}
                  </For>
                </div>
              </Show>
              <div class="max-w-3xl mx-auto flex gap-3 items-end">
                <textarea
                  ref={textareaRef}
                  value={input()}
                  onInput={(e) => {
                    setInput(e.currentTarget.value);
                    autoResize(e.currentTarget);
                  }}
                  onKeyDown={handleTextareaKeydown}
                  onPaste={handlePaste}
                  disabled={!state.ready}
                  placeholder={
                    state.ready
                      ? "Type a message..."
                      : "Connecting..."
                  }
                  rows={1}
                  class="flex-1 resize-none rounded-xl bg-zinc-800/50 border border-zinc-700/50 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-500 focus:outline-none focus:border-zinc-500 disabled:opacity-50 disabled:cursor-not-allowed"
                />
                <Show
                  when={isStreaming()}
                  fallback={
                    <button
                      onClick={handleSubmit}
                      disabled={!state.ready || (!input().trim() && pendingImages().length === 0)}
                      class="rounded-xl w-10 h-10 flex items-center justify-center bg-zinc-700 hover:bg-zinc-600 transition-colors disabled:opacity-40 disabled:cursor-not-allowed shrink-0"
                    >
                      <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M5 12l7-7 7 7M12 5v14" /></svg>
                    </button>
                  }
                >
                  <button
                    onClick={() => interruptCoordinator()}
                    class="rounded-xl w-10 h-10 flex items-center justify-center bg-red-900/50 text-red-300 hover:bg-red-900/70 transition-colors shrink-0"
                    title="Interrupt (Ctrl+C)"
                  >
                    <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M6 6h12v12H6z" /></svg>
                  </button>
                </Show>
              </div>
            </div>
          </div>
        </Show>
      </main>

      <Settings open={settingsOpen()} onClose={() => setSettingsOpen(false)} />
    </div>
  );
}

export default App;
