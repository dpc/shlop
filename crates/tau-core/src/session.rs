//! Tree-structured session history types and the records that persist them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tau_proto::{ConnectionId, Event, LogEventId, SessionId, ToolCallId, ToolName};

/// One persisted chat or tool activity entry belonging to a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionEntry {
    UserMessage {
        text: String,
    },
    AgentMessage {
        text: String,
        /// Provider-supplied reasoning summary captured during the
        /// turn, if any. Persisted alongside the response so resume
        /// can re-render it; intentionally excluded from prompt
        /// replay (see harness `assemble_conversation`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    ToolActivity(ToolActivityRecord),
}

/// One persisted tool activity record associated with a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolActivityRecord {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub outcome: ToolActivityOutcome,
}

/// The persisted outcome of one tool activity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolActivityOutcome {
    Requested {
        arguments: tau_proto::CborValue,
    },
    Result {
        result: tau_proto::CborValue,
    },
    Error {
        message: String,
        details: Option<tau_proto::CborValue>,
    },
}

/// Unique identifier for a node in the session tree.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct NodeId(pub u64);

/// One node in the session tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionNode {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub entry: SessionEntry,
}

/// Tree-structured session history with branching.
///
/// Each entry is a node with a unique ID and parent pointer. The
/// `head` tracks the current position. Branching = moving head to an
/// earlier node; the next append creates a new branch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionTree {
    pub(crate) session_id: SessionId,
    pub(crate) nodes: Vec<SessionNode>,
    pub(crate) head: Option<NodeId>,
}

impl SessionTree {
    /// Returns the session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the current head node ID, if any.
    #[must_use]
    pub fn head(&self) -> Option<NodeId> {
        self.head
    }

    /// Returns a node by ID.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&SessionNode> {
        self.nodes.get(id.0 as usize)
    }

    /// Returns all nodes.
    #[must_use]
    pub fn nodes(&self) -> &[SessionNode] {
        &self.nodes
    }

    /// Returns the entries along the current branch (root to head).
    #[must_use]
    pub fn current_branch(&self) -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut current = self.head;
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(id.0 as usize) {
                path.push(&node.entry);
                current = node.parent_id;
            } else {
                break;
            }
        }
        path.reverse();
        path
    }

    /// Returns the direct children of a node.
    #[must_use]
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.parent_id == Some(id))
            .map(|n| n.id)
            .collect()
    }

    pub(crate) fn append_node(&mut self, entry: SessionEntry) -> NodeId {
        let id = NodeId(self.nodes.len() as u64);
        self.nodes.push(SessionNode {
            id,
            parent_id: self.head,
            entry,
        });
        self.head = Some(id);
        id
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum PersistedSessionRecord {
    Node {
        id: NodeId,
        parent_id: Option<NodeId>,
        entry: SessionEntry,
    },
    SetHead {
        node_id: NodeId,
    },
}

/// One durable session-scoped protocol event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedSessionEvent {
    pub id: LogEventId,
    pub source: Option<ConnectionId>,
    pub event: Event,
}

/// Per-session sidecar metadata at `<state_dir>/<session_id>/meta.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Working directory at the time of session creation.
    pub cwd: Option<PathBuf>,
    /// Unix epoch seconds when the session was first created.
    pub created_at: u64,
    /// Unix epoch seconds of the most recent append.
    pub last_touched: u64,
}
