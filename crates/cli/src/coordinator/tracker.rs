use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Activity update from a running worker agent.
#[derive(Debug, Clone)]
pub enum WorkerActivity {
    ToolStarted(String),
    ToolDone,
    Thinking,
}

pub(super) struct WorkerTracker {
    pub session_to_task: HashMap<String, String>,
    pub last_activity: HashMap<String, Instant>,
    pub current_tool: HashMap<String, String>,
    pub thinking: HashSet<String>,
    pub spawn_time: HashMap<String, Instant>,
}

impl WorkerTracker {
    pub fn new() -> Self {
        Self {
            session_to_task: HashMap::new(),
            last_activity: HashMap::new(),
            current_tool: HashMap::new(),
            thinking: HashSet::new(),
            spawn_time: HashMap::new(),
        }
    }

    pub fn register(&mut self, session_id: String, task_id: String) {
        self.session_to_task
            .insert(session_id.clone(), task_id);
        self.last_activity.insert(session_id.clone(), Instant::now());
        self.spawn_time.insert(session_id, Instant::now());
    }

    pub fn remove(&mut self, session_id: &str) -> Option<Instant> {
        self.session_to_task.remove(session_id);
        self.last_activity.remove(session_id);
        self.current_tool.remove(session_id);
        self.thinking.remove(session_id);
        self.spawn_time.remove(session_id)
    }

    /// Build the worker list for MonitorTick: (session_id, task_id, last_activity).
    pub fn worker_list(&self) -> Vec<(String, String, Instant)> {
        self.last_activity
            .iter()
            .filter_map(|(sid, last)| {
                let tid = self.session_to_task.get(sid)?.clone();
                Some((sid.clone(), tid, *last))
            })
            .collect()
    }

    pub fn worker_count(&self) -> usize {
        self.session_to_task.len()
    }
}
