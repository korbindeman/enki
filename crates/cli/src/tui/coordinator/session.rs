use enki_acp::AgentManager;
use tokio::sync::mpsc;

use super::FromCoordinator;

pub(super) type PromptResult = (u64, Result<String, String>);

pub(super) struct CoordinatorSession {
    pub session_id: String,
    pub pending_events: Vec<String>,
    prompt_generation: u64,
    active_prompt: Option<tokio::task::JoinHandle<()>>,
    prompt_done_tx: mpsc::UnboundedSender<PromptResult>,
    pub forward_updates: std::rc::Rc<std::cell::Cell<bool>>,
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

        self.spawn_prompt(mgr, full_text);
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
        let events_text = std::mem::take(&mut self.pending_events).join("\n");
        let msg = format!("[worker status updates]\n{events_text}");
        self.spawn_prompt(mgr, msg);
    }

    fn spawn_prompt(&mut self, mgr: &AgentManager, text: String) {
        self.prompt_generation += 1;
        let generation = self.prompt_generation;
        let mgr = mgr.clone();
        let sid = self.session_id.clone();
        let done_tx = self.prompt_done_tx.clone();
        self.active_prompt = Some(tokio::task::spawn_local(async move {
            let result = mgr.prompt(&sid, &text).await;
            let _ = done_tx.send((generation, result.map_err(|e| e.to_string())));
        }));
    }
}
