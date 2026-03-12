use std::time::{Duration, Instant};

use enki_acp::acp_schema as acp;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use enki_acp::AgentManager;
use tokio::sync::mpsc;

use super::FromCoordinator;

pub(super) type PromptResult = (u64, Result<String, String>);

/// What triggered the last prompt sent to the coordinator agent.
enum PromptSource {
    /// A user-typed message.
    User,
    /// An automatic flush of pending worker events.
    EventFlush,
    /// No prompt has been sent yet (session startup).
    None,
}

pub(super) struct CoordinatorSession {
    pub session_id: String,
    pub pending_events: Vec<String>,
    prompt_generation: u64,
    active_prompt: Option<tokio::task::JoinHandle<()>>,
    prompt_done_tx: mpsc::UnboundedSender<PromptResult>,
    pub forward_updates: std::rc::Rc<std::cell::Cell<bool>>,
    last_prompt_source: PromptSource,
    idle_since: Option<Instant>,
}

impl CoordinatorSession {
    pub fn new(session_id: String) -> (Self, mpsc::UnboundedReceiver<PromptResult>) {
        let (prompt_done_tx, prompt_done_rx) = mpsc::unbounded_channel();
        let session = Self {
            session_id,
            pending_events: Vec::new(),
            prompt_generation: 0,
            active_prompt: None,
            prompt_done_tx,
            forward_updates: std::rc::Rc::new(std::cell::Cell::new(false)),
            last_prompt_source: PromptSource::None,
            idle_since: None,
        };
        (session, prompt_done_rx)
    }

    pub fn queue_event(&mut self, msg: String) {
        self.pending_events.push(msg);
    }

    pub async fn deliver_prompt(
        &mut self,
        mgr: &AgentManager,
        tx: &mpsc::UnboundedSender<FromCoordinator>,
        text: String,
        images: Vec<super::ImageData>,
    ) {
        if let Some(handle) = self.active_prompt.take() {
            let _ = mgr.cancel(&self.session_id).await;
            handle.abort();
            let _ = tx.send(FromCoordinator::Interrupted);
        }

        let full_text = if self.pending_events.is_empty() {
            text
        } else {
            let events_text = std::mem::take(&mut self.pending_events).join("\n");
            format!("[worker status updates]\n{events_text}\n\n[user message]\n{text}")
        };

        let content = build_content_blocks(full_text, images);
        self.last_prompt_source = PromptSource::User;
        self.idle_since = None;
        self.spawn_prompt(mgr, content);
    }

    pub fn handle_prompt_done(
        &mut self,
        generation: u64,
        result: Result<String, String>,
    ) -> Option<FromCoordinator> {
        if generation != self.prompt_generation {
            return None;
        }
        self.active_prompt = None;
        self.idle_since = Some(Instant::now());
        Some(match result {
            Ok(stop_reason) => FromCoordinator::Done(stop_reason),
            Err(e) => FromCoordinator::Error(format!("prompt error: {e}")),
        })
    }

    pub async fn interrupt(
        &mut self,
        mgr: &AgentManager,
        tx: &mpsc::UnboundedSender<FromCoordinator>,
    ) {
        if let Some(handle) = self.active_prompt.take() {
            let _ = mgr.cancel(&self.session_id).await;
            handle.abort();
            let _ = tx.send(FromCoordinator::Interrupted);
        }
    }

    pub fn shutdown(&mut self, mgr: &AgentManager) {
        if let Some(handle) = self.active_prompt.take() {
            handle.abort();
        }
        mgr.kill_session(&self.session_id);
    }

    pub fn flush_if_idle(&mut self, mgr: &AgentManager) {
        if self.active_prompt.is_some() || self.pending_events.is_empty() {
            return;
        }

        match self.last_prompt_source {
            PromptSource::EventFlush => {
                // Last response was to system events — flush immediately.
                self.do_flush(mgr);
            }
            PromptSource::User => {
                // Last response was to user — wait 10s idle before flushing.
                if let Some(idle_since) = self.idle_since {
                    if idle_since.elapsed() >= Duration::from_secs(10) {
                        self.do_flush(mgr);
                    }
                }
            }
            PromptSource::None => {
                // No prompt sent yet — wait for first interaction.
            }
        }
    }

    fn do_flush(&mut self, mgr: &AgentManager) {
        let events_text = std::mem::take(&mut self.pending_events).join("\n");
        let msg = format!("[worker status updates]\n{events_text}");
        let content = vec![acp::ContentBlock::Text(acp::TextContent::new(msg))];
        self.last_prompt_source = PromptSource::EventFlush;
        self.idle_since = None;
        self.spawn_prompt(mgr, content);
    }

    fn spawn_prompt(&mut self, mgr: &AgentManager, content: Vec<acp::ContentBlock>) {
        self.prompt_generation += 1;
        let generation = self.prompt_generation;
        let mgr = mgr.clone();
        let sid = self.session_id.clone();
        let done_tx = self.prompt_done_tx.clone();
        self.active_prompt = Some(tokio::task::spawn_local(async move {
            let result = mgr.prompt(&sid, content).await;
            let _ = done_tx.send((generation, result.map_err(|e| e.to_string())));
        }));
    }
}

/// Build ACP content blocks from text and optional images.
fn build_content_blocks(text: String, images: Vec<super::ImageData>) -> Vec<acp::ContentBlock> {
    let mut blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
    for image in images {
        let data = BASE64.encode(&image.bytes);
        blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(data, image.mime_type)));
    }
    blocks
}
