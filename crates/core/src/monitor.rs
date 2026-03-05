use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// If no ACP session update arrives for this long, send a cancel signal.
/// Soft timeout — tries to gracefully stop a stale worker before the
/// hard per-tier timeout kills it.
pub const STALE_CANCEL_SECS: u64 = 5 * 60; // 5 minutes

/// Maximum number of times a task can be retried after timeout/stuck failure
/// before the monitor stops retrying and escalates.
pub const MAX_TASK_RETRIES: u32 = 2;

/// Actions the monitor wants the runtime to take.
#[derive(Debug, Clone)]
pub enum MonitorAction {
    /// Send a cancel signal to a stale session.
    CancelSession {
        session_id: String,
        task_id: String,
        stale_secs: u64,
    },
    /// A task has exhausted its retry budget — needs human/coordinator attention.
    Escalation(String),
}

/// Pure state machine tracking worker health and retry budgets.
///
/// Input: worker activity data. Output: actions.
/// No async, no DB, no ACP dependency.
pub struct MonitorState {
    /// task_id → number of times this task has been retried after stuck/timeout.
    retry_counts: HashMap<String, u32>,
    /// session_ids that have already been cancelled by the monitor.
    /// Prevents sending duplicate cancel signals every tick.
    cancelled: HashSet<String>,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorState {
    pub fn new() -> Self {
        Self {
            retry_counts: HashMap::new(),
            cancelled: HashSet::new(),
        }
    }

    /// Check active workers for staleness and return actions.
    /// Each worker is `(session_id, task_id, last_activity_time)`.
    pub fn tick(&mut self, workers: &[(String, String, Instant)]) -> Vec<MonitorAction> {
        let mut actions = Vec::new();
        let now = Instant::now();
        let stale_threshold = Duration::from_secs(STALE_CANCEL_SECS);

        for (session_id, task_id, last_activity) in workers {
            let elapsed = now.duration_since(*last_activity);
            if elapsed <= stale_threshold {
                continue;
            }
            if self.cancelled.contains(session_id.as_str()) {
                continue;
            }

            self.cancelled.insert(session_id.clone());
            actions.push(MonitorAction::CancelSession {
                session_id: session_id.clone(),
                task_id: task_id.clone(),
                stale_secs: elapsed.as_secs(),
            });
        }

        actions
    }

    /// Record a retry attempt for a task. Returns the new count.
    pub fn record_retry(&mut self, task_id: &str) -> u32 {
        let count = self.retry_counts.entry(task_id.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    /// Check if a task has exhausted its retry budget.
    pub fn should_block_retry(&self, task_id: &str) -> bool {
        self.retry_counts.get(task_id).copied().unwrap_or(0) >= MAX_TASK_RETRIES
    }

    /// Clear retry count when a task completes successfully.
    pub fn clear_retries(&mut self, task_id: &str) {
        self.retry_counts.remove(task_id);
    }

    /// Clean up cancelled tracking when a session ends.
    pub fn session_ended(&mut self, session_id: &str) {
        self.cancelled.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_worker_produces_cancel() {
        let mut monitor = MonitorState::new();
        let stale_time = Instant::now() - Duration::from_secs(STALE_CANCEL_SECS + 10);
        let workers = vec![
            ("sess-1".into(), "task-1".into(), stale_time),
        ];
        let actions = monitor.tick(&workers);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], MonitorAction::CancelSession { session_id, .. } if session_id == "sess-1"));
    }

    #[test]
    fn fresh_worker_no_cancel() {
        let mut monitor = MonitorState::new();
        let fresh_time = Instant::now() - Duration::from_secs(10);
        let workers = vec![
            ("sess-1".into(), "task-1".into(), fresh_time),
        ];
        let actions = monitor.tick(&workers);
        assert!(actions.is_empty());
    }

    #[test]
    fn no_duplicate_cancel() {
        let mut monitor = MonitorState::new();
        let stale_time = Instant::now() - Duration::from_secs(STALE_CANCEL_SECS + 10);
        let workers = vec![
            ("sess-1".into(), "task-1".into(), stale_time),
        ];
        let actions1 = monitor.tick(&workers);
        assert_eq!(actions1.len(), 1);

        // Second tick — already cancelled, no action.
        let actions2 = monitor.tick(&workers);
        assert!(actions2.is_empty());
    }

    #[test]
    fn session_ended_allows_recancel() {
        let mut monitor = MonitorState::new();
        let stale_time = Instant::now() - Duration::from_secs(STALE_CANCEL_SECS + 10);
        let workers = vec![
            ("sess-1".into(), "task-1".into(), stale_time),
        ];
        monitor.tick(&workers);
        monitor.session_ended("sess-1");

        let actions = monitor.tick(&workers);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn retry_budget() {
        let mut monitor = MonitorState::new();
        assert!(!monitor.should_block_retry("task-1"));

        assert_eq!(monitor.record_retry("task-1"), 1);
        assert!(!monitor.should_block_retry("task-1"));

        assert_eq!(monitor.record_retry("task-1"), 2);
        assert!(monitor.should_block_retry("task-1"));

        // Clear resets.
        monitor.clear_retries("task-1");
        assert!(!monitor.should_block_retry("task-1"));
    }
}
