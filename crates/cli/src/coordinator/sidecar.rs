use std::collections::VecDeque;
use std::rc::Rc;
use std::cell::Cell;

use enki_acp::acp_schema as acp;
use enki_acp::AgentManager;
use tokio::sync::mpsc;

pub(super) type SidecarResult = (u64, Result<String, String>);

pub(super) struct SidecarSession {
    pub session_id: String,
    prompt_generation: u64,
    active_prompt: Option<tokio::task::JoinHandle<()>>,
    prompt_done_tx: mpsc::UnboundedSender<SidecarResult>,
    task_queue: VecDeque<String>,
    pub forward_updates: Rc<Cell<bool>>,
}

impl SidecarSession {
    pub fn new(session_id: String) -> (Self, mpsc::UnboundedReceiver<SidecarResult>) {
        let (prompt_done_tx, prompt_done_rx) = mpsc::unbounded_channel();
        let session = Self {
            session_id,
            prompt_generation: 0,
            active_prompt: None,
            prompt_done_tx,
            task_queue: VecDeque::new(),
            forward_updates: Rc::new(Cell::new(false)),
        };
        (session, prompt_done_rx)
    }

    pub fn dispatch(&mut self, mgr: &AgentManager, prompt: String) {
        if self.is_idle() {
            self.spawn_prompt(mgr, prompt);
        } else {
            self.task_queue.push_back(prompt);
        }
    }

    pub fn handle_done(
        &mut self,
        mgr: &AgentManager,
        generation: u64,
        result: Result<String, String>,
    ) -> Option<String> {
        if generation != self.prompt_generation {
            return None;
        }
        self.active_prompt = None;

        // Log result status.
        match &result {
            Ok(_) => tracing::info!("sidecar task completed"),
            Err(e) => tracing::warn!(error = %e, "sidecar task failed"),
        }

        // Dequeue next task if any.
        if let Some(next_prompt) = self.task_queue.pop_front() {
            tracing::info!(queue_remaining = self.task_queue.len(), "sidecar dequeuing next task");
            self.spawn_prompt(mgr, next_prompt);
        }

        Some("completed".to_string())
    }

    pub fn is_idle(&self) -> bool {
        self.active_prompt.is_none()
    }

    pub fn shutdown(&mut self, mgr: &AgentManager) {
        if let Some(handle) = self.active_prompt.take() {
            handle.abort();
        }
        self.task_queue.clear();
        mgr.kill_session(&self.session_id);
    }

    fn spawn_prompt(&mut self, mgr: &AgentManager, prompt: String) {
        self.prompt_generation += 1;
        let generation = self.prompt_generation;
        let mgr = mgr.clone();
        let sid = self.session_id.clone();
        let done_tx = self.prompt_done_tx.clone();
        let content = vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))];
        self.active_prompt = Some(tokio::task::spawn_local(async move {
            let result = mgr.prompt(&sid, content).await;
            let _ = done_tx.send((generation, result.map_err(|e| e.to_string())));
        }));
    }
}
