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
  switchAgent,
} from "./store";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { renderMarkdown } from "./markdown";
import { invoke } from "@tauri-apps/api/core";
import type { ContentBlock, Message } from "./types";
import WorkerPanel from "./WorkerPanel";
import TaskList from "./TaskList";
import TierBadge from "./TierBadge";
import Settings from "./Settings";
import Backlog from "./Backlog";
import { Check, GitMerge, AlertTriangle, XCircle, Loader2, Wrench } from "lucide-solid";

interface PendingImage {
  id: string;
  data: string; // base64
  mime_type: string;
  url: string; // data URL for preview
}

const AGENT_DISPLAY_NAMES: Record<string, string> = {
  claude: "Claude Code",
  codex: "OpenAI Codex",
  opencode: "OpenCode",
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
      await switchAgent(key);
    } catch {
      // Ignore if coordinator not ready.
    }
  }

  let containerRef!: HTMLDivElement;

  function onClickOutside(e: MouseEvent) {
    if (open() && !containerRef.contains(e.target as Node)) setOpen(false);
  }
  function onKeyDown(e: KeyboardEvent) {
    if (open() && e.key === "Escape") setOpen(false);
  }

  onMount(() => {
    document.addEventListener("mousedown", onClickOutside);
    document.addEventListener("keydown", onKeyDown);
  });
  onCleanup(() => {
    document.removeEventListener("mousedown", onClickOutside);
    document.removeEventListener("keydown", onKeyDown);
  });

  return (
    <div ref={containerRef} class="relative">
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
  const statusIcon = () => {
    switch (props.card.status) {
      case "running": return <Loader2 class="w-3.5 h-3.5 text-text-muted animate-spin" />;
      case "done": return <Check class="w-3.5 h-3.5 text-emerald-400" />;
      case "merging": return <GitMerge class="w-3.5 h-3.5 text-blue-400 animate-pulse" />;
      case "merged": return <GitMerge class="w-3.5 h-3.5 text-emerald-400" />;
      case "conflicted": return <AlertTriangle class="w-3.5 h-3.5 text-amber-400" />;
      default: return <XCircle class="w-3.5 h-3.5 text-red-400" />;
    }
  };

  return (
    <div class="my-1.5 flex items-center gap-2.5 px-1 py-1 text-xs text-text-muted">
      <span class="shrink-0">{statusIcon()}</span>
      <span class="text-text truncate">{props.card.title}</span>
      <TierBadge tier={props.card.tier} />
      <Show when={props.card.error}>
        <span class="text-red-400 truncate">{props.card.error}</span>
      </Show>
    </div>
  );
}

function ToolCallPanel(props: { block: ContentBlock & { type: "tools" }; isLast: boolean; streaming: boolean }) {
  const [userToggled, setUserToggled] = createSignal(false);
  const [manualOpen, setManualOpen] = createSignal(false);

  // Auto-expand when this is the active (last) tool group in a streaming message.
  // Once it's no longer last (text appeared after), auto-collapse — unless user overrode.
  const expanded = () => {
    if (userToggled()) return manualOpen();
    return props.isLast && props.streaming;
  };

  function toggle() {
    setUserToggled(true);
    setManualOpen(!expanded());
  }

  return (
    <div class="my-2 border border-border-subtle rounded-lg bg-surface/30 overflow-hidden">
      <button
        onClick={toggle}
        class="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-surface/20 transition-colors"
      >
        <Wrench class="w-3 h-3 text-text-muted" />
        <span class="text-[10px] uppercase tracking-wider text-text-muted font-medium">
          Tool calls ({props.block.calls.length})
        </span>
        <svg
          class={`w-3 h-3 text-text-faint transition-transform ml-auto ${expanded() ? "rotate-90" : ""}`}
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          viewBox="0 0 24 24"
        >
          <path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
        </svg>
      </button>
      <Show when={expanded()}>
        <div class="px-4 pb-2">
          <For each={props.block.calls}>
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
    </div>
  );
}

function ChatMessage(props: { message: Message }) {
  const textContent = () => {
    const b = props.message.blocks[0];
    return b?.type === "text" ? b.content : "";
  };

  // For the streaming indicator: find any active tool call across all blocks
  const activeToolCall = () => {
    for (let i = props.message.blocks.length - 1; i >= 0; i--) {
      const block = props.message.blocks[i];
      if (block.type === "tools") {
        const pending = block.calls.find((tc) => !tc.done);
        if (pending) return pending;
      }
    }
    return undefined;
  };

  return (
    <Show
      when={!props.message.workerCard}
      fallback={<WorkerCardView card={props.message.workerCard!} />}
    >
      <Switch>
        <Match when={props.message.role === "system"}>
          <div class="text-xs text-text-muted text-center py-3">
            {textContent()}
          </div>
        </Match>
        <Match when={props.message.role === "user"}>
          <div class="flex justify-end py-3">
            <div class="max-w-[85%] rounded-2xl bg-surface px-4 py-2.5 text-sm whitespace-pre-wrap text-text">
              <Show when={props.message.images?.length}>
                <div class="flex gap-2 flex-wrap mb-2">
                  <For each={props.message.images}>
                    {(url) => (
                      <img src={url} alt="" class="w-32 h-32 rounded-lg object-cover" />
                    )}
                  </For>
                </div>
              </Show>
              {textContent()}
            </div>
          </div>
        </Match>
        <Match when={props.message.role === "assistant"}>
          <div class="py-3">
            <For each={props.message.blocks}>
              {(block, i) => (
                <Switch>
                  <Match when={block.type === "text"}>
                    <div
                      class="prose"
                      innerHTML={renderMarkdown(
                        (block as ContentBlock & { type: "text" }).content,
                        props.message.streaming,
                      )}
                    />
                  </Match>
                  <Match when={block.type === "tools"}>
                    <ToolCallPanel
                      block={block as ContentBlock & { type: "tools" }}
                      isLast={i() === props.message.blocks.length - 1}
                      streaming={props.message.streaming}
                    />
                  </Match>
                </Switch>
              )}
            </For>
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
  const [backlogOpen, setBacklogOpen] = createSignal(false);
  const [stickToBottom, setStickToBottom] = createSignal(true);

  const isStreaming = () => {
    const msgs = state.messages;
    const last = msgs[msgs.length - 1];
    return !!(last?.role === "assistant" && last?.streaming);
  };

  onMount(() => {
    initStore();
    listen("menu-open-project", () => handleOpenProject());
  });

  // Track scroll position to detect user scrolling away from bottom
  function handleScroll() {
    if (!messagesContainer) return;
    const { scrollTop, scrollHeight, clientHeight } = messagesContainer;
    const distanceFromBottom = scrollHeight - scrollTop - clientHeight;
    setStickToBottom(distanceFromBottom < 50);
  }

  onMount(() => messagesContainer?.addEventListener("scroll", handleScroll));
  onCleanup(() => messagesContainer?.removeEventListener("scroll", handleScroll));

  function scrollToBottom() {
    messagesContainer?.scrollTo({
      top: messagesContainer.scrollHeight,
      behavior: "smooth",
    });
    setStickToBottom(true);
  }

  // Auto-scroll on new messages or streaming content updates
  createEffect(() => {
    const len = state.messages.length;
    if (len > 0) {
      const msg = state.messages[len - 1];
      const blen = msg.blocks.length;
      if (blen > 0) {
        const last = msg.blocks[blen - 1];
        // Access reactive properties to track streaming updates
        if (last.type === "text") void last.content;
        if (last.type === "tools") void last.calls.length;
      }
    }
    if (stickToBottom()) {
      messagesContainer?.scrollTo({
        top: messagesContainer.scrollHeight,
        behavior: "smooth",
      });
    }
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
    if ((e.metaKey || e.ctrlKey) && e.key === "b") {
      e.preventDefault();
      setBacklogOpen((v) => !v);
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
          <div class="flex items-center justify-end gap-2">
            <button
              onClick={() => setBacklogOpen(true)}
              class="text-text-muted hover:text-text transition-colors"
              title={`Backlog (${navigator.platform.includes("Mac") ? "\u2318" : "Ctrl"}B)`}
            >
              <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2" />
              </svg>
            </button>
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
          <div class="relative flex-1 overflow-hidden">
            <div ref={messagesContainer} class="h-full overflow-y-auto">
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
            <Show when={!stickToBottom()}>
              <button
                onClick={scrollToBottom}
                class="back-to-bottom absolute top-3 left-1/2 -translate-x-1/2 z-10 flex items-center gap-1.5 rounded-full px-3 py-1.5 text-xs text-text-muted bg-surface/90 border border-border-subtle backdrop-blur-sm hover:text-text hover:bg-surface transition-colors cursor-pointer"
              >
                <svg class="w-3 h-3" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" d="M19 14l-7 7-7-7M12 21V3" />
                </svg>
                Back to latest
              </button>
            </Show>
          </div>

          {/* Input area */}
          <div class="relative">
            <div class="absolute top-0 left-0 right-0 -translate-y-full h-8 pointer-events-none" style="background: linear-gradient(to top, var(--color-background), transparent)" />
            <div class="px-6 py-3 pb-5">
              <div class="max-w-3xl mx-auto rounded-2xl bg-surface/50 border border-border-subtle focus-within:border-border transition-colors">
                {/* Pending images */}
                <Show when={pendingImages().length > 0}>
                  <div class="px-4 pt-3 flex gap-2 flex-wrap">
                    <For each={pendingImages()}>
                      {(img) => (
                        <div class="relative group">
                          <img
                            src={img.url}
                            alt=""
                            class="w-20 h-20 rounded-lg border border-border object-cover"
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
                  class="w-full resize-none bg-transparent px-5 pt-4 pb-3 text-sm text-text placeholder-text-muted focus:outline-none disabled:opacity-50 disabled:cursor-not-allowed"
                />
                {/* Bottom toolbar */}
                <div class="flex items-center justify-between px-4 py-2">
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
      <Backlog open={backlogOpen()} onClose={() => setBacklogOpen(false)} />
    </div>
  );
}

export default App;
