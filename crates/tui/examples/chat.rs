use std::time::Duration;

use enki_tui::chat::{Chat, ChatContext, Handler, UserInput};
use enki_tui::lines;
use tokio::sync::mpsc;

// ─── Backend simulation ──────────────────────────────────────

enum BackendMsg {
    Chunk(String),
    ToolCall(String),
    ToolDone(String),
    Thinking,
    Done,
}

/// Canned responses that cycle through different capabilities.
const RESPONSES: &[&str] = &[
    // 0: Simple markdown
    "Sure, I can help with that.\n\n\
Here's a quick overview:\n\n\
- **First**, we'll read the relevant files\n\
- **Then**, make the necessary changes\n\
- **Finally**, verify everything compiles\n\n\
Let me take a look at your codebase.",

    // 1: Code block
    "Here's the implementation:\n\n\
```rust\n\
pub struct Config {\n\
    pub name: String,\n\
    pub verbose: bool,\n\
}\n\
\n\
impl Config {\n\
    pub fn load() -> Result<Self> {\n\
        let raw = std::fs::read_to_string(\"config.toml\")?;\n\
        toml::from_str(&raw).map_err(Into::into)\n\
    }\n\
}\n\
```\n\n\
This reads the config file and deserializes it. The `Result` propagates any IO or parse errors.",

    // 2: Multi-tool workflow
    "I found the issue. The `process` function isn't handling the edge case \
where the input is empty. Let me fix that.\n\n\
The fix is straightforward — add an early return at the top of the function. \
I've also added a test to cover this case.",

    // 3: Short answer
    "Done. The changes compile and all tests pass.",
];

/// Tools to simulate for each response index.
const TOOL_SEQUENCES: &[&[(&str, u64)]] = &[
    // 0: read files
    &[("Read src/main.rs", 400), ("Read src/lib.rs", 300)],
    // 1: read + write
    &[("Read src/config.rs", 350), ("Write src/config.rs", 200)],
    // 2: multi-step
    &[
        ("Read src/process.rs", 300),
        ("Write src/process.rs", 200),
        ("Read tests/process_test.rs", 250),
        ("Write tests/process_test.rs", 200),
        ("Run cargo test", 800),
    ],
    // 3: quick
    &[("Run cargo check", 500)],
];

async fn fake_respond(tx: mpsc::UnboundedSender<BackendMsg>, index: usize) {
    let resp = RESPONSES[index % RESPONSES.len()];
    let tools = TOOL_SEQUENCES[index % TOOL_SEQUENCES.len()];

    // Thinking
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(BackendMsg::Thinking).ok();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Tool calls
    for (name, duration_ms) in tools {
        tx.send(BackendMsg::ToolCall(name.to_string())).ok();
        tokio::time::sleep(Duration::from_millis(*duration_ms)).await;
        tx.send(BackendMsg::ToolDone(name.to_string())).ok();
    }

    // Stream the response in small chunks
    let bytes = resp.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let chunk_size = (7 + pos % 5).min(bytes.len() - pos);
        let end = (pos + chunk_size).min(bytes.len());
        let end = if end < bytes.len() {
            let mut e = end;
            while e > pos && !resp.is_char_boundary(e) {
                e -= 1;
            }
            if e == pos { end } else { e }
        } else {
            end
        };
        let chunk = &resp[pos..end];
        tx.send(BackendMsg::Chunk(chunk.to_string())).ok();
        pos = end;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tx.send(BackendMsg::Done).ok();
}

// ─── Handler ─────────────────────────────────────────────────

struct ChatApp {
    tx: mpsc::UnboundedSender<BackendMsg>,
    msg_count: usize,
}

impl Handler<BackendMsg> for ChatApp {
    fn on_message(&mut self, msg: BackendMsg, cx: &mut ChatContext) {
        match msg {
            BackendMsg::Thinking => cx.think(),
            BackendMsg::ToolCall(name) => {
                cx.print(&lines::tool_call(&name));
                cx.tool(name);
            }
            BackendMsg::ToolDone(name) => {
                cx.print(&lines::tool_done(&name));
                cx.think();
            }
            BackendMsg::Chunk(text) => cx.stream(&text),
            BackendMsg::Done => {
                cx.finish_markdown();
                cx.blank_line();
                cx.separator();
            }
        }
    }

    fn on_submit(&mut self, _input: UserInput, _cx: &mut ChatContext) {
        let tx = self.tx.clone();
        let idx = self.msg_count;
        self.msg_count += 1;
        tokio::spawn(async move {
            fake_respond(tx, idx).await;
        });
    }
}

// ─── Main ────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let app = ChatApp { tx, msg_count: 0 };

    Chat::new("❯ ")
        .title("enki", "chat")
        .run(app, || rx.try_recv().ok())?;

    Ok(())
}
