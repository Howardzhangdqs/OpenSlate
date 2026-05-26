//! Runtime execution tree tracking actual agent invocations.

use std::collections::HashMap;

use crate::types::{AgentId, ExecutionNodeId, RunId};

/// Status of an execution node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed,
}

/// A node in the execution tree (runtime instance of an [`crate::agent_tree::AgentNode`]).
#[derive(Debug, Clone)]
pub struct ExecutionNode {
    pub id: ExecutionNodeId,
    pub run_id: RunId,
    pub agent_id: AgentId,
    pub parent_execution_id: Option<ExecutionNodeId>,
    pub parent_call_id: Option<String>,
    pub status: ExecutionStatus,
    pub depth: u32,
}

/// The execution tree built during a run.
#[derive(Debug, Clone)]
pub struct ExecutionTree {
    nodes: HashMap<ExecutionNodeId, ExecutionNode>,
    root_id: ExecutionNodeId,
}

impl ExecutionTree {
    /// Create a new execution tree with a root node.
    pub fn new(run_id: RunId, root_agent_id: AgentId) -> Self {
        let root_id = ExecutionNodeId(format!("en-{}", uuid::Uuid::new_v4()));
        let root = ExecutionNode {
            id: root_id.clone(),
            run_id,
            agent_id: root_agent_id,
            parent_execution_id: None,
            parent_call_id: None,
            status: ExecutionStatus::Running,
            depth: 0,
        };
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root);
        Self { nodes, root_id }
    }

    /// Create a child execution node under the given parent.
    ///
    /// Returns the [`ExecutionNodeId`] of the newly created child.
    pub fn create_child(
        &mut self,
        run_id: RunId,
        agent_id: AgentId,
        parent_execution_id: ExecutionNodeId,
        parent_call_id: Option<String>,
    ) -> ExecutionNodeId {
        let parent_depth = self
            .nodes
            .get(&parent_execution_id)
            .map(|n| n.depth)
            .unwrap_or(0);

        let id = ExecutionNodeId(format!("en-{}", uuid::Uuid::new_v4()));
        let node = ExecutionNode {
            id: id.clone(),
            run_id,
            agent_id,
            parent_execution_id: Some(parent_execution_id),
            parent_call_id,
            status: ExecutionStatus::Running,
            depth: parent_depth + 1,
        };
        self.nodes.insert(id.clone(), node);
        id
    }

    /// Look up an execution node by id.
    pub fn get(&self, id: &ExecutionNodeId) -> Option<&ExecutionNode> {
        self.nodes.get(id)
    }

    /// Get the root execution node.
    pub fn root(&self) -> &ExecutionNode {
        self.nodes.get(&self.root_id).expect("root exists")
    }

    /// Get the root execution node id.
    pub fn root_id(&self) -> &ExecutionNodeId {
        &self.root_id
    }

    /// Get the depth of an execution node.
    pub fn depth(&self, id: &ExecutionNodeId) -> Option<u32> {
        self.nodes.get(id).map(|n| n.depth)
    }

    /// Update the status of an execution node.
    pub fn update_status(&mut self, id: &ExecutionNodeId, status: ExecutionStatus) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run_id() -> RunId {
        RunId("r-test".into())
    }

    #[test]
    fn test_root_node_depth_zero() {
        let tree = ExecutionTree::new(make_run_id(), AgentId("root".into()));
        let root = tree.root();
        assert_eq!(root.depth, 0);
        assert_eq!(root.agent_id.0, "root");
        assert!(root.parent_execution_id.is_none());
    }

    #[test]
    fn test_child_depth_one() {
        let mut tree = ExecutionTree::new(make_run_id(), AgentId("root".into()));
        let root_id = tree.root_id().clone();
        let child_id = tree.create_child(
            make_run_id(),
            AgentId("researcher".into()),
            root_id,
            None,
        );
        assert_eq!(tree.depth(&child_id), Some(1));
    }

    #[test]
    fn test_grandchild_depth_two() {
        let mut tree = ExecutionTree::new(make_run_id(), AgentId("root".into()));
        let root_id = tree.root_id().clone();
        let child_id = tree.create_child(
            make_run_id(),
            AgentId("researcher".into()),
            root_id,
            None,
        );
        let grandchild_id = tree.create_child(
            make_run_id(),
            AgentId("verifier".into()),
            child_id,
            Some("call-123".into()),
        );
        assert_eq!(tree.depth(&grandchild_id), Some(2));
        let gc = tree.get(&grandchild_id).expect("grandchild exists");
        assert_eq!(gc.parent_call_id.as_deref(), Some("call-123"));
    }

    #[test]
    fn test_same_agent_multiple_executions() {
        let mut tree = ExecutionTree::new(make_run_id(), AgentId("root".into()));
        let root_id = tree.root_id().clone();
        let exec1 = tree.create_child(
            make_run_id(),
            AgentId("researcher".into()),
            root_id.clone(),
            None,
        );
        let exec2 = tree.create_child(
            make_run_id(),
            AgentId("researcher".into()),
            root_id,
            None,
        );
        assert_ne!(exec1, exec2, "two executions must have different ids");
        assert_eq!(tree.get(&exec1).unwrap().agent_id, tree.get(&exec2).unwrap().agent_id);
    }

    #[test]
    fn test_update_status() {
        let mut tree = ExecutionTree::new(make_run_id(), AgentId("root".into()));
        let root_id = tree.root_id().clone();
        assert_eq!(tree.root().status, ExecutionStatus::Running);

        tree.update_status(&root_id, ExecutionStatus::Completed);
        assert_eq!(tree.root().status, ExecutionStatus::Completed);

        // Updating a non-existent id does nothing (no panic)
        let fake_id = ExecutionNodeId("en-nonexistent".into());
        tree.update_status(&fake_id, ExecutionStatus::Failed);
    }
}
