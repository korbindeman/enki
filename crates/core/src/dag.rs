use std::collections::HashMap;

use crate::template::Template;
use crate::types::Tier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Pending,
    Ready,
    Running,
    Done,
    Failed,
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
    /// Build a DAG from a rendered template.
    /// The template must already be validated (no cycles, no unknown deps).
    pub fn from_template(template: &Template) -> Self {
        let mut nodes = Vec::with_capacity(template.steps.len());
        let mut index = HashMap::new();

        // Create nodes
        for (i, step) in template.steps.iter().enumerate() {
            index.insert(step.id.clone(), i);
            nodes.push(Node {
                id: step.id.clone(),
                title: step.title.clone(),
                description: step.description.clone(),
                tier: step.tier,
                status: NodeStatus::Pending,
                deps: Vec::new(),
                dependents: Vec::new(),
            });
        }

        // Wire up edges
        for step in &template.steps {
            let node_idx = index[&step.id];
            for dep_id in &step.needs {
                let dep_idx = index[dep_id];
                nodes[node_idx].deps.push(dep_idx);
                nodes[dep_idx].dependents.push(node_idx);
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

    /// Mark a node as failed.
    pub fn mark_failed(&mut self, id: &str) -> bool {
        if let Some(&idx) = self.index.get(id) {
            if self.nodes[idx].status == NodeStatus::Running {
                self.nodes[idx].status = NodeStatus::Failed;
                return true;
            }
        }
        false
    }

    /// Is every node done?
    pub fn is_complete(&self) -> bool {
        self.nodes.iter().all(|n| n.status == NodeStatus::Done)
    }

    /// Is any node failed?
    pub fn has_failures(&self) -> bool {
        self.nodes.iter().any(|n| n.status == NodeStatus::Failed)
    }

    /// Re-evaluate pending nodes to see if they're now ready.
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
    use crate::template::Template;
    use std::collections::HashMap;

    const SHINY: &str = r#"
name = "shiny"
description = "Design before code, review before ship"

[vars.feature]
description = "Feature to implement"
required = true

[[steps]]
id = "design"
title = "Design"
description = "Think about architecture for {{feature}}"

[[steps]]
id = "implement"
title = "Implement"
description = "Implement {{feature}} based on the design"
needs = ["design"]

[[steps]]
id = "test"
title = "Test"
description = "Write tests for {{feature}}"
needs = ["design"]

[[steps]]
id = "review"
title = "Review"
description = "Review implementation and tests for {{feature}}"
needs = ["implement", "test"]

[[steps]]
id = "merge"
title = "Merge"
description = "Merge {{feature}} to main"
needs = ["review"]
"#;

    fn make_dag() -> Dag {
        let template = Template::from_toml(SHINY).unwrap();
        let mut vars = HashMap::new();
        vars.insert("feature".into(), "auth".into());
        let rendered = template.render(&vars).unwrap();
        Dag::from_template(&rendered)
    }

    #[test]
    fn initial_ready_nodes() {
        let dag = make_dag();
        // Only "design" has no deps, so it should be ready
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
        // implement and test can run in parallel
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
        // review not ready yet — test still running
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
        // Nothing becomes ready after a failure
        assert!(dag.ready_nodes().is_empty());
    }

    #[test]
    fn invalid_transitions() {
        let mut dag = make_dag();
        // Can't mark a pending node as done
        assert!(!dag.mark_done("design"));
        // Can't mark a non-ready node as running
        assert!(!dag.mark_running("implement"));
    }

    #[test]
    fn node_lookup() {
        let dag = make_dag();
        let design = dag.get("design").unwrap();
        assert_eq!(design.title, "Design");
        assert_eq!(design.description, "Think about architecture for auth");
        assert!(design.deps.is_empty());
        assert_eq!(design.dependents.len(), 2); // implement and test
    }

    #[test]
    fn linear_template() {
        let toml = r#"
name = "linear"
description = "sequential steps"

[[steps]]
id = "a"
title = "A"
description = "first"

[[steps]]
id = "b"
title = "B"
description = "second"
needs = ["a"]

[[steps]]
id = "c"
title = "C"
description = "third"
needs = ["b"]
"#;
        let template = Template::from_toml(toml).unwrap();
        let mut dag = Dag::from_template(&template);

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
