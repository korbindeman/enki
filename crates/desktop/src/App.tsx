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
import { invoke } from "@tauri-apps/api/core";
import type { Message } from "./types";
import WorkerPanel from "./WorkerPanel";
import TaskList from "./TaskList";
import TierBadge from "./TierBadge";
import Settings from "./Settings";

interface PendingImage {
  id: string;
  data: string; // base64
  mime_type: string;
  url: string; // data URL for preview
}

const AGENT_DISPLAY_NAMES: Record<string, string> = {
  claude: "Claude Code",
  codex: "OpenAI Codex",
};

const AGENT_KEYS = Object.keys(AGENT_DISPLAY_NAMES);

function AgentSelector() {
  const [agent, setAgent] = createSignal("claude");
  const [open, setOpen] = createSignal(false);

  onMount(async () => {
    try {
      const config = await invoke<{ agent_command?: string }>("load_config");
      if (config.agent_command && config.agent_command in AGENT_DISPLAY_NAMES) {
        setAgent(config.agent_command);
      }
    } catch {
      // Config not available yet — keep default.
    }
  });

  async function select(key: string) {
    setAgent(key);
    setOpen(false);
    try {
      await invoke("set_agent", { agent: key });
    } catch {
      // Command may not exist yet — ignore.
    }
  }

  return (
    <div class="relative">
      <button
        onClick={() => setOpen((v) => !v)}
        class="flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs text-text-muted hover:text-text hover:bg-surface/50 transition-colors"
      >
        {AGENT_DISPLAY_NAMES[agent()]}
        <svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" d="M19 9l-7 7-7-7" />
        </svg>
      </button>
      <Show when={open()}>
        <div class="absolute bottom-full left-0 mb-1 rounded-lg bg-surface border border-border shadow-xl py-1 min-w-[140px] z-10">
          <For each={AGENT_KEYS}>
            {(key) => (
              <button
                onClick={() => select(key)}
                class={`w-full text-left px-3 py-1.5 text-xs transition-colors ${
                  key === agent()
                    ? "text-text bg-surface/50"
                    : "text-text-muted hover:text-text hover:bg-surface/30"
                }`}
              >
                {AGENT_DISPLAY_NAMES[key]}
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function WorkerCardView(props: { card: NonNullable<Message["workerCard"]> }) {
  const statusDotClass = () => {
    switch (props.card.status) {
      case "running": return "bg-emerald-400 animate-pulse";
      case "done": return "bg-blue-400";
      case "merging": return "bg-blue-400 animate-pulse";
      case "merged": return "bg-emerald-400";
      case "conflicted": return "bg-amber-400";
      default: return "bg-red-400";
    }
  };

  const statusText = () => {
    switch (props.card.status) {
      case "running": return "Running";
      case "done": return "Done";
      case "merging": return "Merging";
      case "merged": return "Merged";
      case "conflicted": return "Conflict";
      default: return "Failed";
    }
  };

  const statusTextClass = () => {
    switch (props.card.status) {
      case "merged": return "text-emerald-400";
      case "failed": return "text-red-400";
      case "conflicted": return "text-amber-400";
      default: return "text-zinc-500";
    }
  };

  return (
    <div class="my-2 rounded-lg border border-zinc-700/50 bg-zinc-800/30 px-4 py-3">
      <div class="flex items-center gap-3">
        <span class={`inline-block w-2 h-2 rounded-full shrink-0 ${statusDotClass()}`} />
        <span class="text-sm text-zinc-200 flex-1 truncate">{props.card.title}</span>
        <TierBadge tier={props.card.tier} />
        <span class={`text-xs ${statusTextClass()}`}>{statusText()}</span>
      </div>
      <Show when={props.card.error}>
        <div class="mt-2 text-xs text-red-400 truncate">{props.card.error}</div>
      </Show>
    </div>
  );
}

function ChatMessage(props: { message: Message }) {
  const activeToolCall = () =>
    props.message.toolCalls.find((tc) => !tc.done);

  return (
    <Show
      when={!props.message.workerCard}
      fallback={<WorkerCardView card={props.message.workerCard!} />}
    >
      <Switch>
        <Match when={props.message.role === "system"}>
          <div class="text-xs text-text-muted text-center py-3">
            {props.message.content}
          </div>
        </Match>
        <Match when={props.message.role === "user"}>
          <div class="border-l-2 border-border pl-4 py-3">
            <div class="text-sm whitespace-pre-wrap text-text">
              {props.message.content}
            </div>
          </div>
        </Match>
        <Match when={props.message.role === "assistant"}>
          <div class="py-3">
            <Show when={props.message.content}>
              <div
                class="prose"
                innerHTML={renderMarkdown(
                  props.message.content,
                  props.message.streaming,
                )}
              />
            </Show>
            {/* Inline tool calls */}
            <Show when={props.message.toolCalls.length > 0}>
              <div class="mt-3 border border-border-subtle rounded-lg bg-surface/30 px-4 py-3">
                <div class="text-[10px] uppercase tracking-wider text-text-muted font-medium mb-2">
                  Tool calls ({props.message.toolCalls.length})
                </div>
                <For each={props.message.toolCalls}>
                  {(tc) => (
                    <div class="flex items-center gap-2 py-0.5">
                      <span
                        class={`inline-block w-1.5 h-1.5 rounded-full shrink-0 ${tc.done ? "bg-text-faint" : "bg-amber-500 animate-pulse"}`}
                      />
                      <span class="text-xs text-text-muted font-mono truncate">
                        {tc.name}
                      </span>
                    </div>
                  )}
                </For>
              </div>
            </Show>
            {/* Inline status indicator */}
            <Show when={props.message.streaming}>
              <div class="mt-2 flex items-center gap-2">
                <Show
                  when={activeToolCall()}
                  fallback={
                    <>
                      <span class="inline-block w-1.5 h-1.5 rounded-full bg-text-muted animate-pulse" />
                      <span class="text-xs text-text-muted">Responding...</span>
                    </>
                  }
                >
                  <span class="inline-block w-1.5 h-1.5 rounded-full bg-amber-500 animate-pulse" />
                  <span class="text-xs text-text-muted">
                    Using {activeToolCall()!.name}
                  </span>
                </Show>
              </div>
            </Show>
          </div>
        </Match>
      </Switch>
    </Show>
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
    <div class="flex h-screen bg-background text-text">
      {/* Sidebar */}
      <aside class="w-[260px] shrink-0 border-r border-border bg-paper flex flex-col">
        <div class="px-4 py-3">
          <div class="flex items-center justify-between">
            <h1 class="text-lg font-semibold">Enki</h1>
            <button
              onClick={() => setSettingsOpen(true)}
              class="text-text-muted hover:text-text transition-colors"
              title="Settings (⌘,)"
            >
              <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
              </svg>
            </button>
          </div>
          <Show when={state.projectCwd}>
            <div class="text-xs text-text-muted truncate mt-0.5 flex items-center gap-1.5">
              <span title={state.projectCwd!}>{state.projectCwd!.split("/").pop()}</span>
              <Show when={state.currentBranch}>
                <span class="text-text-faint">/</span>
                <span class="text-text-muted">{state.currentBranch}</span>
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
                <h2 class="text-lg font-semibold text-text">No project open</h2>
                <p class="text-sm text-text-muted">Use File &gt; Open Project, or click below.</p>
                <button
                  onClick={handleOpenProject}
                  class="rounded-lg bg-button-bg px-5 py-2.5 text-sm font-medium hover:bg-button-hover transition-colors"
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
                <div class="text-text-muted text-sm pt-20 text-center">
                  Start a conversation to begin orchestrating...
                </div>
              </Show>
              <For each={state.messages}>
                {(msg) => <ChatMessage message={msg} />}
              </For>
            </div>
          </div>

          {/* Input area */}
          <div class="relative">
            <div class="absolute top-0 left-0 right-0 -translate-y-full h-8 pointer-events-none" style="background: linear-gradient(to top, var(--color-background), transparent)" />
            <div class="px-6 py-3 pb-5">
              <div class="max-w-3xl mx-auto rounded-2xl bg-surface/50 border border-border-subtle focus-within:border-text-muted transition-colors">
                {/* Pending images */}
                <Show when={pendingImages().length > 0}>
                  <div class="px-4 pt-3 flex gap-2 flex-wrap">
                    <For each={pendingImages()}>
                      {(img) => (
                        <div class="relative group">
                          <img
                            src={img.url}
                            alt=""
                            class="h-20 rounded-lg border border-border object-cover"
                          />
                          <button
                            onClick={() => removeImage(img.id)}
                            class="absolute -top-1.5 -right-1.5 w-5 h-5 rounded-full bg-button-bg border border-border text-text flex items-center justify-center text-xs hover:bg-red-900 hover:border-red-700 hover:text-red-300 transition-colors opacity-0 group-hover:opacity-100"
                          >
                            &times;
                          </button>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
                {/* Textarea */}
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
                  class="w-full resize-none bg-transparent px-4 py-3 text-sm text-text placeholder-text-muted focus:outline-none disabled:opacity-50 disabled:cursor-not-allowed"
                />
                {/* Bottom toolbar */}
                <div class="flex items-center justify-between px-3 py-2">
                  {/* Agent selector */}
                  <AgentSelector />
                  {/* Send / Stop button */}
                  <Show
                    when={isStreaming()}
                    fallback={
                      <button
                        onClick={handleSubmit}
                        disabled={!state.ready || (!input().trim() && pendingImages().length === 0)}
                        class="rounded-lg w-8 h-8 flex items-center justify-center bg-button-bg hover:bg-button-hover transition-colors disabled:opacity-40 disabled:cursor-not-allowed shrink-0"
                      >
                        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M5 12l7-7 7 7M12 5v14" /></svg>
                      </button>
                    }
                  >
                    <button
                      onClick={() => interruptCoordinator()}
                      class="rounded-lg w-8 h-8 flex items-center justify-center bg-red-900/50 text-red-300 hover:bg-red-900/70 transition-colors shrink-0"
                      title="Interrupt (Ctrl+C)"
                    >
                      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M6 6h12v12H6z" /></svg>
                    </button>
                  </Show>
                </div>
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
