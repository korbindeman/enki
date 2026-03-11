use std::collections::HashMap;

/// Maximum number of times a task can be retried after timeout/stuck failure
/// before the monitor stops retrying and escalates.
pub const MAX_TASK_RETRIES: u32 = 2;

/// Pure state machine tracking retry budgets.
///
/// No async, no DB, no ACP dependency.
pub struct MonitorState {
    /// task_id → number of times this task has been retried after stuck/timeout.
    retry_counts: HashMap<String, u32>,
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
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
