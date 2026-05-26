//! Static agent configuration tree built from agents.yaml.

use std::collections::{HashMap, HashSet};

use crate::types::{AgentConfig, AgentId};

/// A node in the static agent tree.
#[derive(Debug, Clone)]
pub struct AgentNode {
    pub id: AgentId,
    pub name: String,
    pub model_alias: String,
    pub children: Vec<AgentId>,
    pub tools: Vec<String>,
    pub default_prompt: String,
}

/// The complete static agent tree built from configuration.
#[derive(Debug, Clone)]
pub struct AgentTree {
    nodes: HashMap<AgentId, AgentNode>,
    root_id: AgentId,
}

/// Error during tree construction.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("no root agent found")]
    NoRoot,
    #[error("multiple root agents: {0:?}")]
    MultipleRoots(Vec<String>),
    #[error("duplicate agent id: {0}")]
    DuplicateId(String),
    #[error("agent '{agent}' references non-existent child '{child}'")]
    InvalidChildRef { agent: String, child: String },
    #[error("cycle detected: agent '{0}' is both ancestor and descendant")]
    CycleDetected(String),
}

impl AgentTree {
    /// Build an [`AgentTree`] from a list of [`AgentConfig`].
    pub fn from_configs(agents: &[AgentConfig]) -> Result<Self, TreeError> {
        let mut nodes = HashMap::new();

        // Build nodes, check for duplicates
        for agent in agents {
            if nodes.contains_key(&agent.id) {
                return Err(TreeError::DuplicateId(agent.id.0.clone()));
            }
            nodes.insert(
                agent.id.clone(),
                AgentNode {
                    id: agent.id.clone(),
                    name: agent.name.clone(),
                    model_alias: agent.model.clone(),
                    children: agent.children.clone(),
                    tools: agent.tools.clone(),
                    default_prompt: agent.default_prompt.clone(),
                },
            );
        }

        // Find root(s) — agents that are NOT children of any other agent
        let all_children: HashSet<&AgentId> = agents.iter().flat_map(|a| a.children.iter()).collect();
        let roots: Vec<&AgentConfig> = agents
            .iter()
            .filter(|a| !all_children.contains(&a.id))
            .collect();

        if roots.is_empty() {
            return Err(TreeError::NoRoot);
        }
        if roots.len() > 1 {
            return Err(TreeError::MultipleRoots(
                roots.iter().map(|r| r.id.0.clone()).collect(),
            ));
        }

        let root_id = roots[0].id.clone();

        // Validate child references
        for agent in agents {
            for child_id in &agent.children {
                if !nodes.contains_key(child_id) {
                    return Err(TreeError::InvalidChildRef {
                        agent: agent.id.0.clone(),
                        child: child_id.0.clone(),
                    });
                }
            }
        }

        // Check for cycles via DFS
        fn has_cycle(
            id: &AgentId,
            nodes: &HashMap<AgentId, AgentNode>,
            visiting: &mut HashSet<AgentId>,
        ) -> bool {
            if visiting.contains(id) {
                return true;
            }
            visiting.insert(id.clone());
            if let Some(node) = nodes.get(id) {
                for child in &node.children {
                    if has_cycle(child, nodes, visiting) {
                        return true;
                    }
                }
            }
            visiting.remove(id);
            false
        }

        let mut visiting = HashSet::new();
        if has_cycle(&root_id, &nodes, &mut visiting) {
            return Err(TreeError::CycleDetected(root_id.0.clone()));
        }

        Ok(Self { nodes, root_id })
    }

    /// Look up an agent node by id.
    pub fn get_agent(&self, id: &AgentId) -> Option<&AgentNode> {
        self.nodes.get(id)
    }

    /// Get the child agent nodes of a given agent.
    pub fn get_children(&self, id: &AgentId) -> Vec<&AgentNode> {
        self.nodes
            .get(id)
            .map(|n| {
                n.children
                    .iter()
                    .filter_map(|cid| self.nodes.get(cid))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the root agent node.
    pub fn get_root(&self) -> &AgentNode {
        self.nodes.get(&self.root_id).expect("root always exists")
    }

    /// Get the root agent id.
    pub fn root_id(&self) -> &AgentId {
        &self.root_id
    }

    /// Get all agent nodes.
    pub fn all_agents(&self) -> Vec<&AgentNode> {
        self.nodes.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build the example config from agents.yaml.
    fn example_configs() -> Vec<AgentConfig> {
        vec![
            AgentConfig {
                id: AgentId("root".into()),
                name: "Root Agent".into(),
                model: "main".into(),
                children: vec![
                    AgentId("researcher".into()),
                    AgentId("writer".into()),
                    AgentId("deep-analyst".into()),
                    AgentId("visual-inspector".into()),
                ],
                tools: vec!["current_time".into(), "read_file".into(), "list_dir".into()],
                default_prompt: "You are the root coordinator agent.".into(),
            },
            AgentConfig {
                id: AgentId("researcher".into()),
                name: "Researcher".into(),
                model: "fast".into(),
                children: vec![AgentId("verifier".into())],
                tools: vec!["read_file".into(), "current_time".into()],
                default_prompt: "You are a research agent.".into(),
            },
            AgentConfig {
                id: AgentId("verifier".into()),
                name: "Verifier".into(),
                model: "fast".into(),
                children: vec![],
                tools: vec!["read_file".into()],
                default_prompt: "You are a verification agent.".into(),
            },
            AgentConfig {
                id: AgentId("writer".into()),
                name: "Writer".into(),
                model: "fast".into(),
                children: vec![],
                tools: vec![],
                default_prompt: "You are a writing agent.".into(),
            },
            AgentConfig {
                id: AgentId("deep-analyst".into()),
                name: "Deep Analyst".into(),
                model: "deep-reasoner".into(),
                children: vec![],
                tools: vec!["read_file".into()],
                default_prompt: "You are a deep analysis agent.".into(),
            },
            AgentConfig {
                id: AgentId("visual-inspector".into()),
                name: "Visual Inspector".into(),
                model: "vision".into(),
                children: vec![],
                tools: vec!["read_file".into()],
                default_prompt: "You are a visual inspection agent.".into(),
            },
        ]
    }

    #[test]
    fn test_from_example_config() {
        let tree = AgentTree::from_configs(&example_configs()).expect("tree should build");
        assert_eq!(tree.root_id().0, "root");
        assert_eq!(tree.all_agents().len(), 6);
    }

    #[test]
    fn test_root_has_children() {
        let tree = AgentTree::from_configs(&example_configs()).expect("tree should build");
        let root = tree.get_root();
        assert_eq!(root.children.len(), 4);
        let children = tree.get_children(&root.id);
        let child_ids: Vec<&str> = children.iter().map(|c| c.id.0.as_str()).collect();
        assert!(child_ids.contains(&"researcher"));
        assert!(child_ids.contains(&"writer"));
        assert!(child_ids.contains(&"deep-analyst"));
        assert!(child_ids.contains(&"visual-inspector"));
    }

    #[test]
    fn test_get_nonexistent_agent() {
        let tree = AgentTree::from_configs(&example_configs()).expect("tree should build");
        assert!(tree.get_agent(&AgentId("no-such-agent".into())).is_none());
    }

    #[test]
    fn test_duplicate_id_error() {
        let mut configs = example_configs();
        // Clone the first agent (duplicate id)
        configs.push(configs[0].clone());
        let err = AgentTree::from_configs(&configs).unwrap_err();
        assert!(
            matches!(err, TreeError::DuplicateId(ref s) if s == "root"),
            "expected DuplicateId(\"root\"), got {err:?}"
        );
    }

    #[test]
    fn test_no_root_error() {
        // A→B, B→A — every agent is a child of another, so no root
        let configs = vec![
            AgentConfig {
                id: AgentId("a".into()),
                name: "A".into(),
                model: "m".into(),
                children: vec![AgentId("b".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
            AgentConfig {
                id: AgentId("b".into()),
                name: "B".into(),
                model: "m".into(),
                children: vec![AgentId("a".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
        ];
        // This will actually hit cycle detection since the DFS finds a cycle
        // before NoRoot. The root-finding logic will also find no root since
        // all agents are children. Let's verify which error fires first.
        let result = AgentTree::from_configs(&configs);
        // Root-finding happens before cycle detection, so NoRoot should fire
        assert!(
            matches!(result, Err(TreeError::NoRoot)),
            "expected NoRoot, got {result:?}"
        );
    }

    #[test]
    fn test_invalid_child_ref() {
        let configs = vec![
            AgentConfig {
                id: AgentId("root".into()),
                name: "Root".into(),
                model: "m".into(),
                children: vec![AgentId("ghost".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
        ];
        let err = AgentTree::from_configs(&configs).unwrap_err();
        assert!(
            matches!(
                err,
                TreeError::InvalidChildRef {
                    ref agent,
                    ref child
                } if agent == "root" && child == "ghost"
            ),
            "expected InvalidChildRef, got {err:?}"
        );
    }

    #[test]
    fn test_cycle_detection() {
        // root → a → b → a  (cycle via a↔b)
        let configs = vec![
            AgentConfig {
                id: AgentId("root".into()),
                name: "Root".into(),
                model: "m".into(),
                children: vec![AgentId("a".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
            AgentConfig {
                id: AgentId("a".into()),
                name: "A".into(),
                model: "m".into(),
                children: vec![AgentId("b".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
            AgentConfig {
                id: AgentId("b".into()),
                name: "B".into(),
                model: "m".into(),
                children: vec![AgentId("a".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
        ];
        let err = AgentTree::from_configs(&configs).unwrap_err();
        assert!(
            matches!(err, TreeError::CycleDetected(ref s) if s == "root"),
            "expected CycleDetected(\"root\"), got {err:?}"
        );
    }
}
