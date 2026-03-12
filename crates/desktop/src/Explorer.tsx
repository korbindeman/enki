import { createSignal, For, Show, onMount } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { highlightCode, renderMarkdown } from "./markdown";

// ---------------------------------------------------------------------------
// Types (matching Rust structs from bridge.rs)
// ---------------------------------------------------------------------------

interface FileEntry {
  name: string;
  is_dir: boolean;
  size: number;
  child_count: number | null;
  is_hidden: boolean;
  is_gitignored: boolean;
}

interface DirectoryListing {
  path: string;
  entries: FileEntry[];
}

interface TextFileContent {
  content: string;
  language: string;
  is_markdown: boolean;
}

interface ImageFileContent {
  data: string;
  mime_type: string;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const IMAGE_EXTENSIONS = new Set([
  ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".bmp",
]);

function isImageFile(name: string): boolean {
  const dot = name.lastIndexOf(".");
  if (dot < 0) return false;
  return IMAGE_EXTENSIONS.has(name.slice(dot).toLowerCase());
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function fileIcon(entry: FileEntry): string {
  if (entry.is_dir) return "\u{1F4C1}";
  const dot = entry.name.lastIndexOf(".");
  if (dot >= 0 && IMAGE_EXTENSIONS.has(entry.name.slice(dot).toLowerCase())) return "\u{1F5BC}\uFE0F";
  return "\u{1F4C4}";
}

function pathSegments(path: string): { name: string; path: string }[] {
  const parts = path.split("/").filter(Boolean);
  const segments: { name: string; path: string }[] = [];
  for (let i = 0; i < parts.length; i++) {
    segments.push({
      name: parts[i],
      path: "/" + parts.slice(0, i + 1).join("/"),
    });
  }
  return segments;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export default function Explorer(props: { open: boolean; onClose: () => void }) {
  const [currentPath, setCurrentPath] = createSignal("");
  const [entries, setEntries] = createSignal<FileEntry[]>([]);
  const [showHidden, setShowHidden] = createSignal(false);
  const [showGitignored, setShowGitignored] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [history, setHistory] = createSignal<string[]>([]);

  // File viewer state
  const [viewingFile, setViewingFile] = createSignal<{ name: string; path: string } | null>(null);
  const [textContent, setTextContent] = createSignal<TextFileContent | null>(null);
  const [imageContent, setImageContent] = createSignal<ImageFileContent | null>(null);
  const [fileError, setFileError] = createSignal<string | null>(null);
  const [fileLoading, setFileLoading] = createSignal(false);
  const [markdownRendered, setMarkdownRendered] = createSignal(true);

  // Initialize on first open
  onMount(async () => {
    try {
      const cwd = await invoke<string>("get_project_dir");
      if (cwd) {
        setCurrentPath(cwd);
        await loadDirectory(cwd);
      }
    } catch {
      // Project not open yet.
    }
  });

  async function loadDirectory(path: string) {
    setLoading(true);
    setError(null);
    try {
      const listing = await invoke<DirectoryListing>("list_directory", { path });
      setCurrentPath(listing.path);
      setEntries(listing.entries);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function navigateTo(path: string) {
    setHistory((h) => [...h, currentPath()]);
    setViewingFile(null);
    setTextContent(null);
    setImageContent(null);
    setFileError(null);
    loadDirectory(path);
  }

  function goBack() {
    if (viewingFile()) {
      setViewingFile(null);
      setTextContent(null);
      setImageContent(null);
      setFileError(null);
      return;
    }
    const h = history();
    if (h.length > 0) {
      const prev = h[h.length - 1];
      setHistory(h.slice(0, -1));
      loadDirectory(prev);
    } else {
      // Go to parent directory
      const parent = currentPath().replace(/\/[^/]+\/?$/, "") || "/";
      if (parent !== currentPath()) {
        loadDirectory(parent);
      }
    }
  }

  async function openFile(entry: FileEntry) {
    const filePath = currentPath() + "/" + entry.name;
    setViewingFile({ name: entry.name, path: filePath });
    setTextContent(null);
    setImageContent(null);
    setFileError(null);
    setFileLoading(true);
    setMarkdownRendered(true);

    try {
      if (isImageFile(entry.name)) {
        const content = await invoke<ImageFileContent>("read_image_file", { path: filePath });
        setImageContent(content);
      } else {
        const content = await invoke<TextFileContent>("read_text_file", { path: filePath });
        setTextContent(content);
      }
    } catch (e) {
      setFileError(String(e));
    } finally {
      setFileLoading(false);
    }
  }

  function handleItemClick(entry: FileEntry) {
    if (entry.is_dir) {
      navigateTo(currentPath() + "/" + entry.name);
    } else {
      openFile(entry);
    }
  }

  const filteredEntries = () => {
    return entries().filter((e) => {
      if (e.is_hidden && !showHidden()) return false;
      if (e.is_gitignored && !showGitignored()) return false;
      return true;
    });
  };

  function handleBackdrop(e: MouseEvent) {
    if (e.target === e.currentTarget) props.onClose();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (viewingFile()) {
        setViewingFile(null);
        setTextContent(null);
        setImageContent(null);
        setFileError(null);
      } else {
        props.onClose();
      }
    }
  }

  return (
    <Show when={props.open}>
      <div
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
        onClick={handleBackdrop}
        onKeyDown={handleKeydown}
      >
        <div class="explorer-panel w-[720px] max-h-[80vh] bg-surface border border-border-subtle rounded-xl shadow-xl flex flex-col overflow-hidden">
          {/* Navigation bar */}
          <div class="explorer-nav flex items-center gap-2 px-4 py-2.5 border-b border-border-subtle bg-surface shrink-0">
            <button
              onClick={goBack}
              class="explorer-nav-btn w-7 h-7 flex items-center justify-center rounded text-text-muted hover:text-text hover:bg-button-hover transition-colors"
              title="Back"
            >
              <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" d="M15 19l-7-7 7-7" />
              </svg>
            </button>

            {/* Breadcrumb */}
            <div class="flex-1 flex items-center gap-0.5 min-w-0 overflow-x-auto text-sm">
              <For each={pathSegments(currentPath())}>
                {(seg, i) => (
                  <>
                    <Show when={i() > 0}>
                      <span class="text-text-faint shrink-0">/</span>
                    </Show>
                    <button
                      onClick={() => navigateTo(seg.path)}
                      class="explorer-breadcrumb shrink-0 px-1 py-0.5 rounded text-text-muted hover:text-text hover:underline transition-colors truncate max-w-[160px]"
                      title={seg.path}
                    >
                      {seg.name}
                    </button>
                  </>
                )}
              </For>
            </div>

            {/* Toggle buttons */}
            <div class="flex items-center gap-1.5 shrink-0">
              <button
                onClick={() => setShowHidden((v) => !v)}
                class={`explorer-toggle text-[11px] px-2 py-1 rounded-full border transition-colors ${
                  showHidden()
                    ? "bg-button-hover border-border text-text"
                    : "bg-transparent border-border-subtle text-text-faint hover:text-text-muted hover:border-border"
                }`}
              >
                Hidden
              </button>
              <button
                onClick={() => setShowGitignored((v) => !v)}
                class={`explorer-toggle text-[11px] px-2 py-1 rounded-full border transition-colors ${
                  showGitignored()
                    ? "bg-button-hover border-border text-text"
                    : "bg-transparent border-border-subtle text-text-faint hover:text-text-muted hover:border-border"
                }`}
              >
                Gitignored
              </button>
            </div>
          </div>

          {/* Content area */}
          <div class="flex-1 overflow-y-auto">
            <Show
              when={!viewingFile()}
              fallback={<FileViewer
                file={viewingFile()!}
                textContent={textContent()}
                imageContent={imageContent()}
                fileError={fileError()}
                loading={fileLoading()}
                markdownRendered={markdownRendered()}
                onToggleMarkdown={() => setMarkdownRendered((v) => !v)}
                onBack={() => {
                  setViewingFile(null);
                  setTextContent(null);
                  setImageContent(null);
                  setFileError(null);
                }}
              />}
            >
              {/* Loading */}
              <Show when={loading()}>
                <div class="flex items-center justify-center py-12">
                  <span class="text-sm text-text-muted">Loading...</span>
                </div>
              </Show>

              {/* Error */}
              <Show when={error()}>
                <div class="px-4 py-8 text-center text-sm text-red">{error()}</div>
              </Show>

              {/* Directory grid */}
              <Show when={!loading() && !error()}>
                <Show
                  when={filteredEntries().length > 0}
                  fallback={
                    <div class="py-12 text-center text-sm text-text-muted">
                      Empty directory
                    </div>
                  }
                >
                  <div class="explorer-grid p-3">
                    <For each={filteredEntries()}>
                      {(entry) => (
                        <button
                          onClick={() => handleItemClick(entry)}
                          class={`explorer-item flex flex-col items-center gap-1.5 p-3 rounded-lg hover:bg-button-hover transition-colors text-center ${
                            (entry.is_hidden || entry.is_gitignored) ? "opacity-40" : ""
                          }`}
                        >
                          <span class="text-2xl leading-none">{fileIcon(entry)}</span>
                          <span class="text-xs text-text truncate w-full" title={entry.name}>
                            {entry.name}
                          </span>
                          <span class="text-[10px] text-text-faint">
                            {entry.is_dir
                              ? `${entry.child_count ?? 0} items`
                              : formatSize(entry.size)}
                          </span>
                        </button>
                      )}
                    </For>
                  </div>
                </Show>
              </Show>
            </Show>
          </div>
        </div>
      </div>
    </Show>
  );
}

// ---------------------------------------------------------------------------
// File Viewer sub-component
// ---------------------------------------------------------------------------

function FileViewer(props: {
  file: { name: string; path: string };
  textContent: TextFileContent | null;
  imageContent: ImageFileContent | null;
  fileError: string | null;
  loading: boolean;
  markdownRendered: boolean;
  onToggleMarkdown: () => void;
  onBack: () => void;
}) {
  return (
    <div class="flex flex-col h-full">
      {/* File header */}
      <div class="flex items-center gap-2 px-4 py-2 border-b border-border-subtle shrink-0">
        <button
          onClick={props.onBack}
          class="w-6 h-6 flex items-center justify-center rounded text-text-muted hover:text-text hover:bg-button-hover transition-colors"
        >
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" d="M15 19l-7-7 7-7" />
          </svg>
        </button>
        <span class="text-sm text-text font-medium truncate">{props.file.name}</span>
        {/* Markdown toggle */}
        <Show when={props.textContent?.is_markdown}>
          <button
            onClick={props.onToggleMarkdown}
            class="ml-auto text-[11px] px-2 py-0.5 rounded border border-border-subtle text-text-muted hover:text-text hover:border-border transition-colors"
          >
            {props.markdownRendered ? "Source" : "Rendered"}
          </button>
        </Show>
      </div>

      {/* File content */}
      <div class="flex-1 overflow-auto">
        {/* Loading */}
        <Show when={props.loading}>
          <div class="flex items-center justify-center py-12">
            <span class="text-sm text-text-muted">Loading file...</span>
          </div>
        </Show>

        {/* Error */}
        <Show when={props.fileError}>
          <div class="px-6 py-8">
            <div class="text-sm text-text font-medium mb-1">{props.file.name}</div>
            <div class="text-sm text-text-muted">{props.fileError}</div>
          </div>
        </Show>

        {/* Text file */}
        <Show when={!props.loading && !props.fileError && props.textContent}>
          <Show
            when={props.textContent!.is_markdown && props.markdownRendered}
            fallback={
              <div class="explorer-code-view p-4">
                <div
                  class="explorer-code-block"
                  innerHTML={highlightCode(props.textContent!.content, props.textContent!.language)}
                />
              </div>
            }
          >
            <div class="prose px-6 py-4" innerHTML={renderMarkdown(props.textContent!.content)} />
          </Show>
        </Show>

        {/* Image file */}
        <Show when={!props.loading && !props.fileError && props.imageContent}>
          <div class="flex flex-col items-center gap-3 p-6">
            <img
              src={`data:${props.imageContent!.mime_type};base64,${props.imageContent!.data}`}
              alt={props.file.name}
              class="max-w-full max-h-[60vh] object-contain rounded border border-border-subtle"
            />
            <span class="text-xs text-text-muted">{props.file.name}</span>
          </div>
        </Show>
      </div>
    </div>
  );
}
