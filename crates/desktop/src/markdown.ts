import { Marked } from "marked";
import { createHighlighter, type Highlighter } from "shiki";

let highlighter: Highlighter | null = null;

const LANGS = [
  "rust",
  "typescript",
  "javascript",
  "tsx",
  "jsx",
  "python",
  "bash",
  "shell",
  "json",
  "toml",
  "yaml",
  "css",
  "html",
  "sql",
  "go",
  "markdown",
  "diff",
  "c",
  "cpp",
  "java",
  "ruby",
  "swift",
  "kotlin",
  "zig",
  "lua",
];

// Initialize in background — falls back to plain text until ready.
createHighlighter({
  themes: ["github-light", "github-dark-dimmed"],
  langs: LANGS,
}).then((h) => {
  highlighter = h;
});

function escapeHtml(str: string): string {
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

const COPY_ICON = `<svg class="icon-copy" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>`;
const CHECK_ICON = `<svg class="icon-check" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6L9 17l-5-5"/></svg>`;

export function highlightCode(text: string, lang?: string): string {
  const resolved =
    lang && highlighter?.getLoadedLanguages().includes(lang) ? lang : undefined;

  if (!highlighter || !resolved) {
    return `<pre class="shiki"><code>${escapeHtml(text)}</code></pre>`;
  }

  return highlighter.codeToHtml(text, {
    lang: resolved,
    themes: { light: "github-light", dark: "github-dark-dimmed" },
  });
}

const marked = new Marked({
  renderer: {
    code({ text, lang }) {
      const html = highlightCode(text, lang || undefined);
      return `<div class="code-block">${html}<button class="copy-btn" aria-label="Copy code">${COPY_ICON}${CHECK_ICON}</button></div>`;
    },
  },
});

export function renderMarkdown(
  content: string,
  streaming: boolean = false,
): string {
  let src = content;
  if (streaming) {
    // Close any unclosed code fences so partial blocks render properly
    const fenceCount = (src.match(/^```/gm) || []).length;
    if (fenceCount % 2 !== 0) {
      src += "\n```";
    }
  }
  return marked.parse(src, { async: false, breaks: true }) as string;
}

// Delegated copy button handler
document.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest(".copy-btn") as HTMLElement | null;
  if (!btn) return;
  const block = btn.closest(".code-block");
  const code = block?.querySelector("code");
  if (!code) return;
  navigator.clipboard.writeText(code.textContent || "");
  btn.classList.add("copied");
  setTimeout(() => btn.classList.remove("copied"), 2000);
});
