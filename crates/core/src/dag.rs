use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::types::Tier;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCondition {
    /// Dep's merge must have landed (node status == Done). This is the default.
    #[default]
    Merged,
    /// Dep's worker must have finished (node status == WorkerDone or Done).
    Completed,
    /// Dep just needs to have started running (node status == Running, WorkerDone, or Done).
    Started,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub target: usize,
    pub condition: EdgeCondition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Ready,
    Running,
    /// Worker finished, merge pending.
    WorkerDone,
    /// Merge landed.
    Done,
    Failed,
    Blocked,
    Paused,
    Cancelled,
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::WorkerDone => "worker_done",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub description: String,
    pub tier: Option<Tier>,
    pub status: NodeStatus,
    pub checkpoint: bool,
    /// Agent role for this step (e.g. "researcher", "feature_developer").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Edges to nodes this one depends on.
    pub deps: Vec<Edge>,
    /// Indices of nodes that depend on this one.
    pub dependents: Vec<usize>,
}

/// A directed acyclic graph tracking execution of a rendered template.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
                checkpoint: false,
                role: None,
                deps: Vec::new(),
                dependents: Vec::new(),
            }],
            index,
        }
    }

    /// Build a DAG from raw step data (e.g. reconstructed from the DB).
    /// Each tuple is (step_id, title, description, tier, dep_step_ids).
    /// All edges default to Merged condition.
    #[allow(clippy::type_complexity)]
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
                checkpoint: false,
                role: None,
                deps: Vec::new(),
                dependents: Vec::new(),
            });
        }

        for (step_id, _, _, _, dep_ids) in steps {
            let node_idx = index[step_id];
            for dep_id in dep_ids {
                if let Some(&dep_idx) = index.get(dep_id) {
                    nodes[node_idx].deps.push(Edge {
                        target: dep_idx,
                        condition: EdgeCondition::Merged,
                    });
                    nodes[dep_idx].dependents.push(node_idx);
                }
            }
        }

        let mut dag = Dag { nodes, index };
        dag.evaluate_ready();
        dag
    }

    /// Build a DAG from step data with explicit edge conditions.
    /// Each tuple is (step_id, title, description, tier, checkpoint, role, deps).
    #[allow(clippy::type_complexity)]
    pub fn from_steps_with_edges(
        steps: &[(
            String,
            String,
            String,
            Option<Tier>,
            bool,
            Option<String>,
            Vec<(String, EdgeCondition)>,
        )],
    ) -> Self {
        let mut nodes = Vec::with_capacity(steps.len());
        let mut index = HashMap::new();

        for (i, (step_id, title, description, tier, checkpoint, role, _)) in
            steps.iter().enumerate()
        {
            index.insert(step_id.clone(), i);
            nodes.push(Node {
                id: step_id.clone(),
                title: title.clone(),
                description: description.clone(),
                tier: *tier,
                status: NodeStatus::Pending,
                checkpoint: *checkpoint,
                role: role.clone(),
                deps: Vec::new(),
                dependents: Vec::new(),
            });
        }

        for (step_id, _, _, _, _, _, dep_list) in steps {
            let node_idx = index[step_id];
            for (dep_id, condition) in dep_list {
                if let Some(&dep_idx) = index.get(dep_id) {
                    nodes[node_idx].deps.push(Edge {
                        target: dep_idx,
                        condition: *condition,
                    });
                    nodes[dep_idx].dependents.push(node_idx);
                }
            }
        }

        let mut dag = Dag { nodes, index };
        dag.evaluate_ready();
        dag
    }

    /// Append new steps to a running DAG. Returns Err if any dep references
    /// an unknown node or if adding the steps would create a cycle.
    #[allow(clippy::type_complexity)]
    pub fn add_steps(
        &mut self,
        steps: &[(
            String,
            String,
            String,
            Option<Tier>,
            bool,
            Option<String>,
            Vec<(String, EdgeCondition)>,
        )],
    ) -> Result<(), String> {
        // Validate: no duplicate IDs with existing nodes or among new steps.
        let mut new_ids: HashSet<&str> = HashSet::new();
        for (id, ..) in steps {
            if self.index.contains_key(id) {
                return Err(format!("step id '{}' already exists in DAG", id));
            }
            if !new_ids.insert(id) {
                return Err(format!("duplicate step id: {}", id));
            }
        }

        // Validate dep references exist (in existing DAG or new steps).
        for (id, _, _, _, _, _, deps) in steps {
            for (dep_id, _) in deps {
                if !self.index.contains_key(dep_id) && !new_ids.contains(dep_id.as_str()) {
                    return Err(format!(
                        "step '{}' depends on unknown step '{}'",
                        id, dep_id
                    ));
                }
            }
        }

        let base_idx = self.nodes.len();

        // Append nodes.
        for (i, (id, title, description, tier, checkpoint, role, _)) in steps.iter().enumerate() {
            let idx = base_idx + i;
            self.index.insert(id.clone(), idx);
            self.nodes.push(Node {
                id: id.clone(),
                title: title.clone(),
                description: description.clone(),
                tier: *tier,
                status: NodeStatus::Pending,
                checkpoint: *checkpoint,
                role: role.clone(),
                deps: Vec::new(),
                dependents: Vec::new(),
            });
        }

        // Wire up deps.
        for (id, _, _, _, _, _, dep_list) in steps {
            let node_idx = self.index[id];
            for (dep_id, condition) in dep_list {
                let dep_idx = self.index[dep_id];
                self.nodes[node_idx].deps.push(Edge {
                    target: dep_idx,
                    condition: *condition,
                });
                self.nodes[dep_idx].dependents.push(node_idx);
            }
        }

        // Cycle detection.
        if self.has_cycle() {
            // Roll back.
            self.nodes.truncate(base_idx);
            for (id, ..) in steps {
                self.index.remove(id);
            }
            return Err("adding these steps would create a cycle".into());
        }

        self.evaluate_ready();
        Ok(())
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

    /// Mark a node as running. Also re-evaluates ready nodes for Started edges.
    pub fn mark_running(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id)
            && self.nodes[idx].status == NodeStatus::Ready
        {
            self.nodes[idx].status = NodeStatus::Running;
            self.evaluate_ready();
            return true;
        }
        false
    }

    /// Revert a Running node back to Ready (e.g. when the runtime can't
    /// actually spawn the worker yet). Returns true if the revert happened.
    pub fn revert_running(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id)
            && self.nodes[idx].status == NodeStatus::Running
        {
            self.nodes[idx].status = NodeStatus::Ready;
            return true;
        }
        false
    }

    /// Mark a running node as worker-done (worker finished, merge pending).
    pub fn mark_worker_done(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id)
            && self.nodes[idx].status == NodeStatus::Running
        {
            self.nodes[idx].status = NodeStatus::WorkerDone;
            self.evaluate_ready();
            return true;
        }
        false
    }

    /// Mark a node as done (merge landed) and re-evaluate which nodes are now ready.
    pub fn mark_done(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id)
            && matches!(
                self.nodes[idx].status,
                NodeStatus::Running | NodeStatus::WorkerDone
            )
        {
            self.nodes[idx].status = NodeStatus::Done;
            self.evaluate_ready();
            return true;
        }
        false
    }

    /// Mark a node as failed and cascade Blocked to all transitive dependents.
    pub fn mark_failed(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id)
            && matches!(
                self.nodes[idx].status,
                NodeStatus::Running | NodeStatus::WorkerDone
            )
        {
            self.nodes[idx].status = NodeStatus::Failed;
            self.cascade_blocked(idx);
            return true;
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
            NodeStatus::Running | NodeStatus::WorkerDone => {
                self.nodes[idx].status = NodeStatus::Paused;
                Some(true) // caller must kill the worker
            }
            _ => None, // can't pause Done/Failed/Blocked/Cancelled/already Paused
        }
    }

    /// Resume a paused node. Re-evaluates deps first: if all dep edges are
    /// satisfied, the node goes to Ready; otherwise back to Pending.
    pub fn resume_node(&mut self, id: &str) -> bool {
        let Some(&idx) = self.index.get(id) else {
            return false;
        };
        if self.nodes[idx].status != NodeStatus::Paused {
            return false;
        }
        let all_satisfied = self.edges_satisfied(idx);
        self.nodes[idx].status = if all_satisfied {
            NodeStatus::Ready
        } else {
            NodeStatus::Pending
        };
        true
    }

    /// Retry a failed node: reset it (and its blocked dependents) so it can run again.
    /// Returns false if the node isn't in a retryable state (Failed or Blocked).
    pub fn retry_node(&mut self, id: &str) -> bool {
        let Some(&idx) = self.index.get(id) else {
            return false;
        };
        if !matches!(
            self.nodes[idx].status,
            NodeStatus::Failed | NodeStatus::Blocked
        ) {
            return false;
        }

        // Reset the node itself.
        self.nodes[idx].status = NodeStatus::Pending;

        // Un-block transitive dependents that were blocked by this failure.
        let mut stack = self.nodes[idx].dependents.clone();
        while let Some(dep_idx) = stack.pop() {
            if self.nodes[dep_idx].status == NodeStatus::Blocked {
                self.nodes[dep_idx].status = NodeStatus::Pending;
                stack.extend(self.nodes[dep_idx].dependents.iter().copied());
            }
        }

        self.evaluate_ready();
        true
    }

    /// Cancel a node and cascade Cancelled to all transitive dependents
    /// that are Pending/Ready/Blocked/Paused. Returns true if the node
    /// was Running (caller must kill the worker).
    pub fn cancel_node(&mut self, id: &str) -> Option<bool> {
        let &idx = self.index.get(id)?;
        match self.nodes[idx].status {
            NodeStatus::Done | NodeStatus::Cancelled => None,
            NodeStatus::Running | NodeStatus::WorkerDone => {
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

    /// Check if all dep edges for a node are satisfied per their conditions.
    fn edges_satisfied(&self, node_idx: usize) -> bool {
        self.nodes[node_idx].deps.iter().all(|edge| {
            let dep_status = self.nodes[edge.target].status;
            match edge.condition {
                EdgeCondition::Merged => dep_status == NodeStatus::Done,
                EdgeCondition::Completed => {
                    matches!(dep_status, NodeStatus::WorkerDone | NodeStatus::Done)
                }
                EdgeCondition::Started => {
                    matches!(
                        dep_status,
                        NodeStatus::Running | NodeStatus::WorkerDone | NodeStatus::Done
                    )
                }
            }
        })
    }

    /// Re-evaluate pending nodes to see if they're now ready.
    /// Skips Paused and Cancelled nodes.
    fn evaluate_ready(&mut self) {
        for i in 0..self.nodes.len() {
            if self.nodes[i].status != NodeStatus::Pending {
                continue;
            }
            if self.edges_satisfied(i) {
                self.nodes[i].status = NodeStatus::Ready;
            }
        }
    }

    /// Detect cycles using Kahn's algorithm.
    fn has_cycle(&self) -> bool {
        let n = self.nodes.len();
        let mut in_degree: Vec<usize> = self.nodes.iter().map(|node| node.deps.len()).collect();
        let mut queue: VecDeque<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|&(_, d)| *d == 0)
            .map(|(i, _)| i)
            .collect();
        let mut visited = 0;
        while let Some(idx) = queue.pop_front() {
            visited += 1;
            for &dep_idx in &self.nodes[idx].dependents {
                in_degree[dep_idx] -= 1;
                if in_degree[dep_idx] == 0 {
                    queue.push_back(dep_idx);
                }
            }
        }
        visited != n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the "shiny" diamond DAG: design → {implement, test} → review → merge
    fn make_dag() -> Dag {
        Dag::from_steps(&[
            (
                "design".into(),
                "Design".into(),
                "Think about architecture for auth".into(),
                Some(Tier::Heavy),
                vec![],
            ),
            (
                "implement".into(),
                "Implement".into(),
                "Implement auth based on the design".into(),
                Some(Tier::Standard),
                vec!["design".into()],
            ),
            (
                "test".into(),
                "Test".into(),
                "Write tests for auth".into(),
                Some(Tier::Light),
                vec!["design".into()],
            ),
            (
                "review".into(),
                "Review".into(),
                "Review implementation and tests for auth".into(),
                Some(Tier::Standard),
                vec!["implement".into(), "test".into()],
            ),
            (
                "merge".into(),
                "Merge".into(),
                "Merge auth to main".into(),
                Some(Tier::Light),
                vec!["review".into()],
            ),
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
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                vec!["a".into()],
            ),
            (
                "c".into(),
                "C".into(),
                "third".into(),
                None,
                vec!["b".into()],
            ),
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

    // --- WorkerDone tests ---

    #[test]
    fn worker_done_transition() {
        let mut dag = make_dag();
        dag.mark_running("design");

        assert!(dag.mark_worker_done("design"));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::WorkerDone);

        // WorkerDone alone does NOT satisfy Merged edges
        assert!(dag.ready_nodes().is_empty());

        // mark_done from WorkerDone
        assert!(dag.mark_done("design"));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Done);

        // Now Merged dependents become ready
        let mut ready = dag.ready_nodes();
        ready.sort();
        assert_eq!(ready, vec!["implement", "test"]);
    }

    #[test]
    fn worker_done_invalid_from_pending() {
        let mut dag = make_dag();
        assert!(!dag.mark_worker_done("design")); // design is Ready, not Running
    }

    #[test]
    fn mark_done_accepts_running_directly() {
        // Backward compat: mark_done still works from Running (skipping WorkerDone)
        let mut dag = make_dag();
        dag.mark_running("design");
        assert!(dag.mark_done("design"));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Done);
    }

    #[test]
    fn mark_failed_from_worker_done() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_worker_done("design");
        // Failure from WorkerDone (e.g. merge failed)
        assert!(dag.mark_failed("design"));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Failed);
    }

    // --- Edge condition tests ---

    #[test]
    fn completed_edge_fires_on_worker_done() {
        let mut dag = Dag::from_steps_with_edges(&[
            (
                "a".into(),
                "A".into(),
                "first".into(),
                None,
                false,
                None,
                vec![],
            ),
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                false,
                None,
                vec![("a".into(), EdgeCondition::Completed)],
            ),
        ]);

        assert_eq!(dag.ready_nodes(), vec!["a"]);
        dag.mark_running("a");
        assert!(dag.ready_nodes().is_empty());

        // WorkerDone satisfies Completed edge
        dag.mark_worker_done("a");
        assert_eq!(dag.ready_nodes(), vec!["b"]);
    }

    #[test]
    fn started_edge_fires_on_running() {
        let mut dag = Dag::from_steps_with_edges(&[
            (
                "a".into(),
                "A".into(),
                "first".into(),
                None,
                false,
                None,
                vec![],
            ),
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                false,
                None,
                vec![("a".into(), EdgeCondition::Started)],
            ),
        ]);

        assert_eq!(dag.ready_nodes(), vec!["a"]);

        // Running satisfies Started edge
        dag.mark_running("a");
        assert_eq!(dag.ready_nodes(), vec!["b"]);
    }

    #[test]
    fn merged_edge_requires_done() {
        let mut dag = Dag::from_steps_with_edges(&[
            (
                "a".into(),
                "A".into(),
                "first".into(),
                None,
                false,
                None,
                vec![],
            ),
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                false,
                None,
                vec![("a".into(), EdgeCondition::Merged)],
            ),
        ]);

        dag.mark_running("a");
        assert!(dag.ready_nodes().is_empty());

        dag.mark_worker_done("a");
        assert!(dag.ready_nodes().is_empty()); // Merged needs Done, not WorkerDone

        dag.mark_done("a");
        assert_eq!(dag.ready_nodes(), vec!["b"]);
    }

    #[test]
    fn mixed_edge_conditions() {
        // c depends on a (Started) and b (Merged)
        let mut dag = Dag::from_steps_with_edges(&[
            (
                "a".into(),
                "A".into(),
                "first".into(),
                None,
                false,
                None,
                vec![],
            ),
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                false,
                None,
                vec![],
            ),
            (
                "c".into(),
                "C".into(),
                "third".into(),
                None,
                false,
                None,
                vec![
                    ("a".into(), EdgeCondition::Started),
                    ("b".into(), EdgeCondition::Merged),
                ],
            ),
        ]);

        let mut ready = dag.ready_nodes();
        ready.sort();
        assert_eq!(ready, vec!["a", "b"]);

        // Start a — but b not done yet, so c stays pending
        dag.mark_running("a");
        assert!(dag.ready_nodes().contains(&"b"));
        assert!(!dag.ready_nodes().contains(&"c"));

        // Complete b's full cycle
        dag.mark_running("b");
        dag.mark_done("b");
        // Now a is Started and b is Done → c is ready
        assert!(dag.ready_nodes().contains(&"c"));
    }

    // --- add_steps tests ---

    #[test]
    fn add_steps_basic() {
        let mut dag = Dag::from_steps(&[
            ("a".into(), "A".into(), "first".into(), None, vec![]),
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                vec!["a".into()],
            ),
        ]);

        // Complete a, b should be ready
        dag.mark_running("a");
        dag.mark_done("a");
        assert_eq!(dag.ready_nodes(), vec!["b"]);

        // Add c that depends on b
        let result = dag.add_steps(&[(
            "c".into(),
            "C".into(),
            "third".into(),
            None,
            false,
            None,
            vec![("b".into(), EdgeCondition::Merged)],
        )]);
        assert!(result.is_ok());
        assert!(dag.get("c").is_some());
        assert_eq!(dag.get("c").unwrap().status, NodeStatus::Pending);
    }

    #[test]
    fn add_steps_deps_already_done() {
        let mut dag = Dag::from_steps(&[("a".into(), "A".into(), "first".into(), None, vec![])]);

        dag.mark_running("a");
        dag.mark_done("a");

        // Add b that depends on a (already Done) → b should be immediately Ready
        let result = dag.add_steps(&[(
            "b".into(),
            "B".into(),
            "second".into(),
            None,
            false,
            None,
            vec![("a".into(), EdgeCondition::Merged)],
        )]);
        assert!(result.is_ok());
        assert_eq!(dag.get("b").unwrap().status, NodeStatus::Ready);
    }

    #[test]
    fn add_steps_duplicate_id_fails() {
        let mut dag = Dag::from_steps(&[("a".into(), "A".into(), "first".into(), None, vec![])]);

        let result = dag.add_steps(&[(
            "a".into(),
            "A2".into(),
            "duplicate".into(),
            None,
            false,
            None,
            vec![],
        )]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[test]
    fn add_steps_unknown_dep_fails() {
        let mut dag = Dag::from_steps(&[("a".into(), "A".into(), "first".into(), None, vec![])]);

        let result = dag.add_steps(&[(
            "b".into(),
            "B".into(),
            "second".into(),
            None,
            false,
            None,
            vec![("nonexistent".into(), EdgeCondition::Merged)],
        )]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown step"));
    }

    #[test]
    fn add_steps_cycle_detection() {
        // a → b already exists. Try to add c → a where b → c, creating a cycle.
        let mut dag = Dag::from_steps(&[
            ("a".into(), "A".into(), "first".into(), None, vec![]),
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                vec!["a".into()],
            ),
        ]);

        // Add c depending on b, and try to make a depend on c (cycle: a→b→c→a)
        // We can't modify existing edges, but we can create c→a
        let result = dag.add_steps(&[(
            "c".into(),
            "C".into(),
            "third".into(),
            None,
            false,
            None,
            vec![("b".into(), EdgeCondition::Merged)],
        )]);
        assert!(result.is_ok()); // no cycle: a→b→c is fine

        // Now try to add d that depends on c, and also make c depend on d...
        // Actually we can only add new nodes, not modify existing. Let's test
        // a real cycle among new nodes.
        let result = dag.add_steps(&[
            (
                "x".into(),
                "X".into(),
                "".into(),
                None,
                false,
                None,
                vec![("y".into(), EdgeCondition::Merged)],
            ),
            (
                "y".into(),
                "Y".into(),
                "".into(),
                None,
                false,
                None,
                vec![("x".into(), EdgeCondition::Merged)],
            ),
        ]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));

        // Verify rollback: x and y should not exist
        assert!(dag.get("x").is_none());
        assert!(dag.get("y").is_none());
    }

    #[test]
    fn add_steps_among_new_nodes() {
        let mut dag = Dag::from_steps(&[("a".into(), "A".into(), "first".into(), None, vec![])]);

        dag.mark_running("a");
        dag.mark_done("a");

        // Add b and c where c depends on b, and b depends on a
        let result = dag.add_steps(&[
            (
                "b".into(),
                "B".into(),
                "second".into(),
                None,
                false,
                None,
                vec![("a".into(), EdgeCondition::Merged)],
            ),
            (
                "c".into(),
                "C".into(),
                "third".into(),
                None,
                false,
                None,
                vec![("b".into(), EdgeCondition::Merged)],
            ),
        ]);
        assert!(result.is_ok());
        // b's dep a is Done → b is Ready
        assert_eq!(dag.get("b").unwrap().status, NodeStatus::Ready);
        // c's dep b is not Done yet → c stays Pending
        assert_eq!(dag.get("c").unwrap().status, NodeStatus::Pending);
    }

    // --- Checkpoint field test ---

    // --- retry_node tests ---

    #[test]
    fn retry_failed_node() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_failed("design");

        // All dependents should be blocked.
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Failed);
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Blocked);

        // Retry design.
        assert!(dag.retry_node("design"));
        assert_eq!(dag.get("design").unwrap().status, NodeStatus::Ready); // no deps → Ready
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Pending);
        assert_eq!(dag.get("test").unwrap().status, NodeStatus::Pending);
        assert_eq!(dag.get("review").unwrap().status, NodeStatus::Pending);
        assert_eq!(dag.get("merge").unwrap().status, NodeStatus::Pending);
    }

    #[test]
    fn retry_failed_mid_graph() {
        let mut dag = make_dag();
        dag.mark_running("design");
        dag.mark_done("design");
        dag.mark_running("implement");
        dag.mark_running("test");
        dag.mark_failed("implement");

        // test is still running, review/merge are blocked.
        assert_eq!(dag.get("test").unwrap().status, NodeStatus::Running);
        assert_eq!(dag.get("review").unwrap().status, NodeStatus::Blocked);
        assert_eq!(dag.get("merge").unwrap().status, NodeStatus::Blocked);

        // Retry implement.
        assert!(dag.retry_node("implement"));
        assert_eq!(dag.get("implement").unwrap().status, NodeStatus::Ready); // design is Done
        assert_eq!(dag.get("review").unwrap().status, NodeStatus::Pending); // unblocked but deps not met
        assert_eq!(dag.get("merge").unwrap().status, NodeStatus::Pending);
    }

    #[test]
    fn retry_non_failed_node_rejected() {
        let mut dag = make_dag();
        // design is Ready — can't retry.
        assert!(!dag.retry_node("design"));

        dag.mark_running("design");
        // Running — can't retry.
        assert!(!dag.retry_node("design"));

        dag.mark_done("design");
        // Done — can't retry.
        assert!(!dag.retry_node("design"));
    }

    #[test]
    fn retry_unknown_node_rejected() {
        let mut dag = make_dag();
        assert!(!dag.retry_node("nonexistent"));
    }

    #[test]
    fn checkpoint_field_preserved() {
        let dag = Dag::from_steps_with_edges(&[(
            "investigate".into(),
            "Investigate".into(),
            "Look at the problem".into(),
            Some(Tier::Light),
            true, // checkpoint
            None,
            vec![],
        )]);

        assert!(dag.get("investigate").unwrap().checkpoint);
    }
}
