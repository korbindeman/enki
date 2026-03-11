use std::collections::{HashMap, HashSet};

/// Activity update from a running worker agent.
#[derive(Debug, Clone)]
pub enum WorkerActivity {
    ToolStarted(String),
    ToolDone,
    Thinking,
}

pub(super) struct WorkerTracker {
    pub session_to_task: HashMap<String, String>,
    pub current_tool: HashMap<String, String>,
    pub thinking: HashSet<String>,
    pub spawn_time: HashMap<String, std::time::Instant>,
}

impl WorkerTracker {
    pub fn new() -> Self {
        Self {
            session_to_task: HashMap::new(),
            current_tool: HashMap::new(),
            thinking: HashSet::new(),
            spawn_time: HashMap::new(),
        }
    }

    pub fn register(&mut self, session_id: String, task_id: String) {
        self.session_to_task
            .insert(session_id.clone(), task_id);
        self.spawn_time.insert(session_id, std::time::Instant::now());
    }

    pub fn remove(&mut self, session_id: &str) -> Option<std::time::Instant> {
        self.session_to_task.remove(session_id);
        self.current_tool.remove(session_id);
        self.thinking.remove(session_id);
        self.spawn_time.remove(session_id)
    }

    /// Reverse lookup: find the session_id for a given task_id.
    pub fn task_to_session(&self, task_id: &str) -> Option<String> {
        self.session_to_task
            .iter()
            .find(|(_, tid)| tid.as_str() == task_id)
            .map(|(sid, _)| sid.clone())
    }

    pub fn worker_count(&self) -> usize {
        self.session_to_task.len()
    }
}
