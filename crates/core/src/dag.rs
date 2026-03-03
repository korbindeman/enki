use std::collections::HashMap;

use crate::types::Tier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Pending,
    Ready,
    Running,
    Done,
    Failed,
    Blocked,
    Paused,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub description: String,
    pub tier: Option<Tier>,
    pub status: NodeStatus,
    /// Indices of nodes this one depends on.
    pub deps: Vec<usize>,
    /// Indices of nodes that depend on this one.
    pub dependents: Vec<usize>,
}

/// A directed acyclic graph tracking execution of a rendered template.
#[derive(Debug, Clone)]
pub struct Dag {
    nodes: Vec<Node>,
    index: HashMap<String, usize>,
}

impl Dag {
    /// Build a single-node DAG for a standalone task.
    pub fn single(step_id: &str, title: &str, description: &str, tier: Option<Tier>) -> Self {
        let mut index = HashMap::new();
        index.insert(step_id.to_string(), 0);
        Dag {
            nodes: vec![Node {
                id: step_id.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                tier,
                status: NodeStatus::Ready,
                deps: Vec::new(),
                dependents: Vec::new(),
            }],
            index,
        }
    }

    /// Build a DAG from raw step data (e.g. reconstructed from the DB).
    /// Each tuple is (step_id, title, description, tier, dep_step_ids).
    pub fn from_steps(steps: &[(String, String, String, Option<Tier>, Vec<String>)]) -> Self {
        let mut nodes = Vec::with_capacity(steps.len());
        let mut index = HashMap::new();

        for (i, (step_id, title, description, tier, _)) in steps.iter().enumerate() {
            index.insert(step_id.clone(), i);
            nodes.push(Node {
                id: step_id.clone(),
                title: title.clone(),
                description: description.clone(),
                tier: *tier,
                status: NodeStatus::Pending,
                deps: Vec::new(),
                dependents: Vec::new(),
            });
        }

        for (step_id, _, _, _, dep_ids) in steps {
            let node_idx = index[step_id];
            for dep_id in dep_ids {
                if let Some(&dep_idx) = index.get(dep_id) {
                    nodes[node_idx].deps.push(dep_idx);
                    nodes[dep_idx].dependents.push(node_idx);
                }
            }
        }

        let mut dag = Dag { nodes, index };
        dag.evaluate_ready();
        dag
    }

    /// Get a node by step ID.
    pub fn get(&self, id: &str) -> Option<&Node> {
        self.index.get(id).map(|&i| &self.nodes[i])
    }

    /// All nodes in the DAG.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Node IDs that are ready to execute (all deps done, status == Ready).
    pub fn ready_nodes(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Ready)
            .map(|n| n.id.as_str())
            .collect()
    }

    /// Mark a node as running.
    pub fn mark_running(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id) {
            if self.nodes[idx].status == NodeStatus::Ready {
                self.nodes[idx].status = NodeStatus::Running;
                return true;
            }
        }
        false
    }

    /// Mark a node as done and re-evaluate which nodes are now ready.
    pub fn mark_done(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id) {
            if self.nodes[idx].status == NodeStatus::Running {
                self.nodes[idx].status = NodeStatus::Done;
                self.evaluate_ready();
                return true;
            }
        }
        false
    }

    /// Mark a node as failed and cascade Blocked to all transitive dependents.
    pub fn mark_failed(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id) {
            if self.nodes[idx].status == NodeStatus::Running {
                self.nodes[idx].status = NodeStatus::Failed;
                self.cascade_blocked(idx);
                return true;
            }
        }
        false
    }

    /// Node IDs that are blocked (dependency failed).
    pub fn blocked_nodes(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Blocked)
            .map(|n| n.id.as_str())
            .collect()
    }

    /// Pause a node. Ready/Pending → Paused. Returns true if the node was
    /// Running (caller must kill the worker). Returns false if the node
    /// can't be paused (Done/Failed/Cancelled).
    pub fn pause_node(&mut self, id: &str) -> Option<bool> {
        let &idx = self.index.get(id)?;
        match self.nodes[idx].status {
            NodeStatus::Ready | NodeStatus::Pending => {
                self.nodes[idx].status = NodeStatus::Paused;
                Some(false)
            }
            NodeStatus::Running => {
                self.nodes[idx].status = NodeStatus::Paused;
                Some(true) // caller must kill the worker
            }
            _ => None, // can't pause Done/Failed/Blocked/Cancelled/already Paused
        }
    }

    /// Resume a paused node. Re-evaluates deps first: if all deps are done,
    /// the node goes to Ready; otherwise back to Pending.
    pub fn resume_node(&mut self, id: &str) -> bool {
        let Some(&idx) = self.index.get(id) else {
            return false;
        };
        if self.nodes[idx].status != NodeStatus::Paused {
            return false;
        }
        let all_deps_done = self.nodes[idx]
            .deps
            .iter()
            .all(|&dep_idx| self.nodes[dep_idx].status == NodeStatus::Done);
        self.nodes[idx].status = if all_deps_done {
            NodeStatus::Ready
        } else {
            NodeStatus::Pending
        };
        true
    }

    /// Cancel a node and cascade Cancelled to all transitive dependents
    /// that are Pending/Ready/Blocked/Paused. Returns true if the node
    /// was Running (caller must kill the worker).
    pub fn cancel_node(&mut self, id: &str) -> Option<bool> {
        let &idx = self.index.get(id)?;
        match self.nodes[idx].status {
            NodeStatus::Done | NodeStatus::Cancelled => None,
            NodeStatus::Running => {
                self.nodes[idx].status = NodeStatus::Cancelled;
                self.cascade_cancelled(idx);
                Some(true) // caller must kill the worker
            }
            _ => {
                self.nodes[idx].status = NodeStatus::Cancelled;
                self.cascade_cancelled(idx);
                Some(false)
            }
        }
    }

    /// Set a node's status directly (used for crash recovery).
    pub fn set_status(&mut self, id: &str, status: NodeStatus) -> bool {
        if let Some(&idx) = self.index.get(id) {
            self.nodes[idx].status = status;
            return true;
        }
        false
    }

    /// Propagate Blocked status to all transitive dependents of a failed node.
    fn cascade_blocked(&mut self, failed_idx: usize) {
        let mut stack = self.nodes[failed_idx].dependents.clone();
        while let Some(idx) = stack.pop() {
            if matches!(self.nodes[idx].status, NodeStatus::Pending | NodeStatus::Ready) {
                self.nodes[idx].status = NodeStatus::Blocked;
                stack.extend(self.nodes[idx].dependents.iter().copied());
            }
        }
    }

    /// Propagate Cancelled to transitive dependents that haven't completed.
    fn cascade_cancelled(&mut self, cancelled_idx: usize) {
        let mut stack = self.nodes[cancelled_idx].dependents.clone();
        while let Some(idx) = stack.pop() {
            if matches!(
                self.nodes[idx].status,
                NodeStatus::Pending | NodeStatus::Ready | NodeStatus::Blocked | NodeStatus::Paused
            ) {
                self.nodes[idx].status = NodeStatus::Cancelled;
                stack.extend(self.nodes[idx].dependents.iter().copied());
            }
        }
    }

    /// Is every node in a terminal state (Done or Cancelled)?
    pub fn is_complete(&self) -> bool {
        self.nodes
            .iter()
            .all(|n| matches!(n.status, NodeStatus::Done | NodeStatus::Cancelled))
    }

    /// Is any node failed?
    pub fn has_failures(&self) -> bool {
        self.nodes.iter().any(|n| n.status == NodeStatus::Failed)
    }

    /// Public re-evaluation (used after crash recovery to promote pending nodes).
    pub fn reevaluate(&mut self) {
        self.evaluate_ready();
    }

    /// Re-evaluate pending nodes to see if they're now ready.
    /// Skips Paused and Cancelled nodes.
    fn evaluate_ready(&mut self) {
        for i in 0..self.nodes.len() {
            if self.nodes[i].status != NodeStatus::Pending {
                continue;
            }
            let all_deps_done = self.nodes[i]
                .deps
                .iter()
                .all(|&dep_idx| self.nodes[dep_idx].status == NodeStatus::Done);
            if all_deps_done {
                self.nodes[i].status = NodeStatus::Ready;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the "shiny" diamond DAG: design → {implement, test} → review → merge
    fn make_dag() -> Dag {
        Dag::from_steps(&[
            ("design".into(), "Design".into(), "Think about architecture for auth".into(), Some(Tier::Heavy), vec![]),
            ("implement".into(), "Implement".into(), "Implement auth based on the design".into(), Some(Tier::Standard), vec!["design".into()]),
            ("test".into(), "Test".into(), "Write tests for auth".into(), Some(Tier::Light), vec!["design".into()]),
            ("review".into(), "Review".into(), "Review implementation and tests for auth".into(), Some(Tier::Standard), vec!["implement".into(), "test".into()]),
            ("merge".into(), "Merge".into(), "Merge auth to main".into(), Some(Tier::Light), vec!["review".into()]),
        ])
    }

    #[test]
    fn initial_ready_nodes() {
        let dag = make_dag();
        let ready = dag.ready_nodes();
        assert_eq!(ready, vec!["design"]);
    }

    #[test]
    fn parallel_after_design() {
        let mut dag = make_dag();
        dag.mark_running("design");
        assert!(dag.ready_nodes().is_empty());

        dag.mark_done("design");
        let mut ready = dag.ready_nodes();
        ready.sort();
        assert_eq!(ready, vec!["implement", "test"]);
    }

    #[test]
    fn review_after_both() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_done("design");

        dag.mark_running("implement");
        dag.mark_running("test");
        dag.mark_done("implement");
        assert!(dag.ready_nodes().is_empty());

        dag.mark_done("test");
        assert_eq!(dag.ready_nodes(), vec!["review"]);
    }

    #[test]
    fn full_execution() {
        let mut dag = make_dag();

        dag.mark_running("design");
        dag.mark_done("design");

        dag.mark_running("implement");
        dag.mark_running("test");
        dag.mark_done("implement");
        dag.mark_done("test");

        dag.mark_running("review");
        dag.mark_done("review");

        dag.mark_running("merge");
        assert!(!dag.is_complete());
        dag.mark_done("merge");
        assert!(dag.is_complete());
        assert!(!dag.has_failures());
    }

    #[test]
    fn failure_tracking() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_failed("design");
        assert!(dag.has_failures());
        assert!(!dag.is_complete());
        assert!(dag.ready_nodes().is_empty());
    }

    #[test]
    fn failure_cascade_blocks_dependents() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_failed("design");

        let mut blocked = dag.blocked_nodes();
        blocked.sort();
        assert_eq!(blocked, vec!["implement", "merge", "review", "test"]);
    }

    #[test]
    fn failure_cascade_partial() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_done("design");

        dag.mark_running("implement");
        dag.mark_running("test");
        dag.mark_failed("implement");

        assert_eq!(dag.get("test").unwrap().status, NodeStatus::Running);
        let mut blocked = dag.blocked_nodes();
        blocked.sort();
        assert_eq!(blocked, vec!["merge", "review"]);
    }

    #[test]
    fn invalid_transitions() {
        let mut dag = make_dag();
        assert!(!dag.mark_done("design"));
        assert!(!dag.mark_running("implement"));
    }

    #[test]
    fn node_lookup() {
        let dag = make_dag();
        let design = dag.get("design").unwrap();
        assert_eq!(design.title, "Design");
        assert_eq!(design.description, "Think about architecture for auth");
        assert!(design.deps.is_empty());
        assert_eq!(design.dependents.len(), 2);
    }

    #[test]
    fn pause_ready_node() {
        let mut dag = make_dag();
        // design is Ready, pause it
        let was_running = dag.pause_node("design");
        assert_eq!(was_running, Some(false));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Paused);
        assert!(dag.ready_nodes().is_empty());
    }

    #[test]
    fn pause_running_node() {
        let mut dag = make_dag();
        dag.mark_running("design");
        let was_running = dag.pause_node("design");
        assert_eq!(was_running, Some(true)); // caller must kill
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Paused);
    }

    #[test]
    fn pause_done_node_fails() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_done("design");
        assert_eq!(dag.pause_node("design"), None);
    }

    #[test]
    fn resume_paused_node_ready() {
        let mut dag = make_dag();
        dag.pause_node("design");
        assert!(dag.resume_node("design"));
        // design has no deps, so it goes to Ready
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Ready);
    }

    #[test]
    fn resume_paused_node_pending() {
        let mut dag = make_dag();
        // Advance design, then pause implement (whose dep design is not done yet — actually design is done)
        dag.mark_running("design");
        dag.mark_done("design");
        // Now implement is Ready. Pause it, then resume
        dag.pause_node("implement");
        assert!(dag.resume_node("implement"));
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Ready);

        // Pause implement again, mark design as not done (via set_status for testing)
        dag.pause_node("implement");
        dag.set_status("design", NodeStatus::Running);
        assert!(dag.resume_node("implement"));
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Pending);
    }

    #[test]
    fn cancel_node_cascades() {
        let mut dag = make_dag();
        // Cancel design — everything downstream should be cancelled
        let was_running = dag.cancel_node("design");
        assert_eq!(was_running, Some(false));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Cancelled);
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Cancelled);
        assert_eq!(dag.get("test").unwrap().status, NodeStatus::Cancelled);
        assert_eq!(dag.get("review").unwrap().status, NodeStatus::Cancelled);
        assert_eq!(dag.get("merge").unwrap().status, NodeStatus::Cancelled);
    }

    #[test]
    fn cancel_running_node() {
        let mut dag = make_dag();
        dag.mark_running("design");
        let was_running = dag.cancel_node("design");
        assert_eq!(was_running, Some(true)); // caller must kill
    }

    #[test]
    fn cancel_partial_cascades_only_downstream() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_done("design");
        dag.mark_running("implement");
        dag.mark_running("test");

        // Cancel implement — review and merge get cancelled, but test stays running
        dag.cancel_node("implement");
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Cancelled);
        assert_eq!(dag.get("test").unwrap().status, NodeStatus::Running);
        // review depends on implement AND test, so it gets cancelled
        assert_eq!(dag.get("review").unwrap().status, NodeStatus::Cancelled);
        assert_eq!(dag.get("merge").unwrap().status, NodeStatus::Cancelled);
    }

    #[test]
    fn is_complete_with_cancelled() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_done("design");
        dag.mark_running("implement");
        dag.mark_done("implement");
        dag.mark_running("test");
        dag.mark_done("test");
        dag.mark_running("review");
        dag.mark_done("review");
        // Cancel merge instead of running it
        dag.cancel_node("merge");
        assert!(dag.is_complete()); // all Done or Cancelled
    }

    #[test]
    fn linear_dag() {
        let mut dag = Dag::from_steps(&[
            ("a".into(), "A".into(), "first".into(), None, vec![]),
            ("b".into(), "B".into(), "second".into(), None, vec!["a".into()]),
            ("c".into(), "C".into(), "third".into(), None, vec!["b".into()]),
        ]);

        assert_eq!(dag.ready_nodes(), vec!["a"]);
        dag.mark_running("a");
        dag.mark_done("a");
        assert_eq!(dag.ready_nodes(), vec!["b"]);
        dag.mark_running("b");
        dag.mark_done("b");
        assert_eq!(dag.ready_nodes(), vec!["c"]);
        dag.mark_running("c");
        dag.mark_done("c");
        assert!(dag.is_complete());
    }
}
