import { Marked } from "marked";
import hljs from "highlight.js";

const marked = new Marked({
  renderer: {
    code({ text, lang }) {
      const language = lang && hljs.getLanguage(lang) ? lang : undefined;
      const highlighted = language
        ? hljs.highlight(text, { language }).value
        : escapeHtml(text);
      return `<pre><code class="hljs${language ? ` language-${language}` : ""}">${highlighted}</code></pre>`;
    },
  },
});

function escapeHtml(str: string): string {
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

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
