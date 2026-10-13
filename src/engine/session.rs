use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(u64);

impl SessionId {
    pub(crate) fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeRef {
    pub session: SessionId,
    /// Adapter-local address; future sparse backends need not use preorder IDs.
    pub address: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskId {
    pub session: SessionId,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Ready,
}

#[derive(Clone, Copy, Debug)]
pub struct NodeInfo {
    pub reference: NodeRef,
    pub parent: Option<NodeRef>,
    pub depth: u32,
    pub kind: &'static str,
    pub child_count: Option<u64>,
    // Legacy tree adapter metadata, not a storage array reference.
    pub subtree_end: usize,
}

impl NodeInfo {
    pub fn container(&self) -> bool {
        matches!(self.kind, "object" | "array")
    }
    pub fn parent(&self) -> Option<usize> {
        self.parent.map(|p| p.address as usize)
    }
    pub fn end(&self) -> usize {
        self.subtree_end
    }
    pub fn kind(&self) -> &'static str {
        self.kind
    }
}

pub struct ChildPage {
    pub children: Vec<NodeInfo>,
    pub next: Option<u64>,
    pub total: Option<u64>,
    pub(crate) _reservation: super::budget::Reservation,
}
