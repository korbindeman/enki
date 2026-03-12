import { createSignal, createEffect, For, Show } from "solid-js";
import {
  state,
  backlogItems,
  loadBacklog,
  addBacklogItem,
  updateBacklogItem,
  removeBacklogItem,
  sendPrompt,
} from "./store";

function timeAgo(dateStr: string): string {
  const now = Date.now();
  const then = new Date(dateStr).getTime();
  const seconds = Math.floor((now - then) / 1000);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export default function Backlog(props: { open: boolean; onClose: () => void }) {
  const [adding, setAdding] = createSignal(false);
  const [addText, setAddText] = createSignal("");
  const [editingId, setEditingId] = createSignal<string | null>(null);
  const [editText, setEditText] = createSignal("");
  let addRef!: HTMLTextAreaElement;
  let editRef!: HTMLTextAreaElement;

  // Reload when project changes (also runs on first mount)
  createEffect(() => {
    const _cwd = state.projectCwd; // track the dependency
    loadBacklog();
  });

  function handleBackdrop(e: MouseEvent) {
    if (e.target === e.currentTarget) props.onClose();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (adding()) {
        setAdding(false);
        setAddText("");
      } else if (editingId()) {
        setEditingId(null);
        setEditText("");
      } else {
        props.onClose();
      }
    }
  }

  function startAdding() {
    setAdding(true);
    setAddText("");
    requestAnimationFrame(() => addRef?.focus());
  }

  async function submitAdd() {
    const body = addText().trim();
    if (!body) return;
    await addBacklogItem(body);
    setAdding(false);
    setAddText("");
  }

  function handleAddKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitAdd();
    }
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      setAdding(false);
      setAddText("");
    }
  }

  function startEditing(id: string, body: string) {
    setEditingId(id);
    setEditText(body);
    requestAnimationFrame(() => editRef?.focus());
  }

  async function submitEdit() {
    const id = editingId();
    const body = editText().trim();
    if (!id || !body) return;
    await updateBacklogItem(id, body);
    setEditingId(null);
    setEditText("");
  }

  function handleEditKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitEdit();
    }
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      setEditingId(null);
      setEditText("");
    }
  }

  async function handlePick(item: { id: string; body: string }) {
    props.onClose();
    await sendPrompt(item.body);
    await removeBacklogItem(item.id);
  }

  function autoResize(el: HTMLTextAreaElement) {
    el.style.height = "auto";
    el.style.height = Math.min(el.scrollHeight, 200) + "px";
  }

  return (
    <Show when={props.open}>
      <div
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
        onClick={handleBackdrop}
        onKeyDown={handleKeydown}
      >
        <div class="w-[500px] max-h-[80vh] bg-surface border border-border-subtle rounded-xl shadow-xl flex flex-col">
          {/* Header */}
          <div class="flex items-center justify-between px-5 py-4 border-b border-border-subtle">
            <h2 class="text-base font-semibold text-text">Backlog</h2>
            <button
              onClick={startAdding}
              class="text-xs px-2.5 py-1 rounded bg-button-bg border border-border hover:bg-button-hover text-text transition-colors"
            >
              Add
            </button>
          </div>

          {/* List */}
          <div class="flex-1 overflow-y-auto px-5 py-3">
            {/* Add form */}
            <Show when={adding()}>
              <div class="mb-3 pb-3 border-b border-border-subtle">
                <textarea
                  ref={addRef}
                  value={addText()}
                  onInput={(e) => {
                    setAddText(e.currentTarget.value);
                    autoResize(e.currentTarget);
                  }}
                  onKeyDown={handleAddKeydown}
                  rows={2}
                  placeholder="What needs to be done?"
                  class="w-full resize-none bg-input-bg border border-border rounded-lg px-3 py-2 text-sm text-text placeholder-text-muted focus:outline-none focus:border-text-muted"
                />
                <div class="flex items-center justify-end gap-2 mt-2">
                  <span class="text-[10px] text-text-faint mr-auto">
                    {navigator.platform.includes("Mac") ? "\u2318" : "Ctrl"}+Enter to save
                  </span>
                  <button
                    onClick={() => { setAdding(false); setAddText(""); }}
                    class="text-xs px-2 py-1 text-text-muted hover:text-text transition-colors"
                  >
                    Cancel
                  </button>
                  <button
                    onClick={submitAdd}
                    disabled={!addText().trim()}
                    class="text-xs px-2.5 py-1 rounded bg-button-bg border border-border hover:bg-button-hover text-text transition-colors disabled:opacity-40"
                  >
                    Save
                  </button>
                </div>
              </div>
            </Show>

            {/* Items */}
            <Show
              when={backlogItems().length > 0}
              fallback={
                <div class="py-12 text-center text-sm text-text-muted">
                  No backlog items yet.
                </div>
              }
            >
              <For each={backlogItems()}>
                {(item) => (
                  <div class="py-3 border-b border-border-subtle last:border-b-0">
                    <Show
                      when={editingId() !== item.id}
                      fallback={
                        <div>
                          <textarea
                            ref={editRef}
                            value={editText()}
                            onInput={(e) => {
                              setEditText(e.currentTarget.value);
                              autoResize(e.currentTarget);
                            }}
                            onKeyDown={handleEditKeydown}
                            rows={2}
                            class="w-full resize-none bg-input-bg border border-border rounded-lg px-3 py-2 text-sm text-text focus:outline-none focus:border-text-muted"
                          />
                          <div class="flex items-center justify-end gap-2 mt-2">
                            <span class="text-[10px] text-text-faint mr-auto">
                              {navigator.platform.includes("Mac") ? "\u2318" : "Ctrl"}+Enter to save
                            </span>
                            <button
                              onClick={() => { setEditingId(null); setEditText(""); }}
                              class="text-xs px-2 py-1 text-text-muted hover:text-text transition-colors"
                            >
                              Cancel
                            </button>
                            <button
                              onClick={submitEdit}
                              disabled={!editText().trim()}
                              class="text-xs px-2.5 py-1 rounded bg-button-bg border border-border hover:bg-button-hover text-text transition-colors disabled:opacity-40"
                            >
                              Save
                            </button>
                          </div>
                        </div>
                      }
                    >
                      <div class="text-sm text-text whitespace-pre-wrap break-words">
                        {item.body}
                      </div>
                      <div class="flex items-center gap-3 mt-2">
                        <span class="text-[10px] text-text-faint">{timeAgo(item.created_at)}</span>
                        <div class="ml-auto flex items-center gap-1">
                          <button
                            onClick={() => startEditing(item.id, item.body)}
                            class="text-[10px] px-1.5 py-0.5 rounded text-text-muted hover:text-text hover:bg-button-bg transition-colors"
                          >
                            Edit
                          </button>
                          <button
                            onClick={() => removeBacklogItem(item.id)}
                            class="text-[10px] px-1.5 py-0.5 rounded text-text-muted hover:text-red hover:bg-button-bg transition-colors"
                          >
                            Delete
                          </button>
                          <button
                            onClick={() => handlePick(item)}
                            class="text-[10px] px-1.5 py-0.5 rounded text-text-muted hover:text-text hover:bg-button-bg transition-colors"
                          >
                            Pick
                          </button>
                        </div>
                      </div>
                    </Show>
                  </div>
                )}
              </For>
            </Show>
          </div>
        </div>
      </div>
    </Show>
  );
}
