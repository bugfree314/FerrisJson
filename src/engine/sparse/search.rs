use super::*;
use crate::engine::{search::Matcher, tree::TreeNode};
#[derive(Clone, Copy)]
struct Cursor {
    parent: Node,
    ordinal: u64,
    position: u64,
}
impl SparseIndex {
    /// A validated continuation avoids restarting each forward page at a checkpoint.
    pub(crate) fn query_array_page(
        &mut self,
        parent: Node,
        ordinal: u64,
        position: Option<u64>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(Page, u64), String> {
        self.validate(parent, cancel)?;
        if parent.kind != Kind::Array || ordinal >= parent.count {
            return Err("Invalid query array cursor".into());
        }
        let (mut position, mut index) = if let Some(position) = position {
            (position, ordinal)
        } else {
            let (p, i, _) = self.array_boundary(parent, ordinal, false, cancel)?;
            (p, i)
        };
        let count = (parent.count - ordinal).min(64) as usize;
        let lease = self.budget.reserve(
            Resource::Memory,
            count * std::mem::size_of::<Node>(),
            "query array continuation page",
        )?;
        let mut nodes = Vec::with_capacity(count);
        let mut scan = Scanner::new(
            &self.snapshot,
            &mut self.store,
            &self.budget,
            parent.session,
            cancel,
            false,
        )?;
        while index < ordinal + count as u64 {
            let node = scan.indexed_node(position)?;
            if index >= ordinal {
                nodes.push(node);
            }
            position = scan.next_array_boundary(node, parent, index)?;
            index += 1;
        }
        Ok((
            Page {
                nodes,
                next: (index < parent.count).then_some(index),
                total: parent.count,
                _lease: lease,
            },
            position,
        ))
    }
    pub fn read_view(&self) -> Result<Self, String> {
        if !self.complete {
            return Err("Pending: search requires full validation".into());
        }
        Ok(Self {
            snapshot: self.snapshot.clone(),
            store: self.store.read_view(&self.budget)?,
            budget: self.budget.clone(),
            root: self.root,
            header: self.header,
            _state: self
                .budget
                .reserve(Resource::Memory, 64 * 1024, "search session view")?,
            _cache: None,
            complete: true,
        })
    }
    pub(crate) fn search_visit(
        &mut self,
        matcher: &Matcher,
        cancel: &dyn Fn() -> bool,
        visit: &mut dyn FnMut(TreeNode, u64, usize, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.complete {
            return Err("Pending: search requires full validation".into());
        }
        let _workspace = self.budget.reserve(
            Resource::Memory,
            64 * 1024,
            "bounded search traversal stack",
        )?;
        let mut reader = Reader::new(&self.snapshot)?.with_cancellation(cancel);
        let root = self.root;
        let root_match = (matcher.keys() && matcher.text("$"))
            || (!root.kind.container()
                && matcher.values()
                && matcher.range(
                    &mut reader,
                    root.start,
                    root.end,
                    root.kind == Kind::String,
                    cancel,
                )?);
        visit(
            TreeNode::from_sparse(root, self.full_rows(root, cancel)?),
            0,
            0,
            root_match,
        )?;
        if !root.kind.container() {
            return Ok(());
        }
        let mut stack = Vec::with_capacity(128);
        stack.push(Cursor {
            parent: root,
            ordinal: 0,
            position: root.start + 1,
        });
        let mut scan = Scanner::new(
            &self.snapshot,
            &mut self.store,
            &self.budget,
            root.session,
            cancel,
            false,
        )?;
        while let Some(frame) = stack.last().copied() {
            check(cancel)?;
            if frame.ordinal >= frame.parent.count {
                stack.pop();
                continue;
            }
            let mut key = false;
            let node = if frame.parent.kind == Kind::Array {
                if matcher.keys() {
                    key = matcher.text(&frame.ordinal.to_string());
                }
                let n = scan.indexed_node(frame.position)?;
                stack.last_mut().unwrap().position =
                    scan.next_array_boundary(n, frame.parent, frame.ordinal)?;
                n
            } else {
                let container = scan.store.get(frame.parent.container, cancel)?;
                let entry = scan
                    .store
                    .directory_find(container[10], frame.ordinal, false, cancel)?
                    .ok_or("Object child missing")?;
                if entry[1] != frame.ordinal {
                    return Err("Object ordinal mismatch".into());
                }
                let field = scan.store.get(entry[2], cancel)?;
                if field[0] != 3 || field[8] != frame.parent.container {
                    return Err("Object field mismatch".into());
                }
                if matcher.keys() {
                    key = matcher.range(&mut reader, field[9], field[10], true, cancel)?;
                }
                let kind = Kind::decode(field[1])?;
                let count = if kind.container() {
                    scan.store.get(field[4], cancel)?[4]
                } else {
                    0
                };
                Node {
                    session: root.session,
                    start: field[2],
                    end: field[3],
                    kind,
                    container: field[4],
                    count,
                }
            };
            let value = if key || !matcher.values() || node.kind.container() {
                false
            } else {
                matcher.range(
                    &mut reader,
                    node.start,
                    node.end,
                    node.kind == Kind::String,
                    cancel,
                )?
            };
            let depth = stack.len();
            let weight = scan.indexed_weight(node)?;
            visit(
                TreeNode::from_sparse(node, weight),
                frame.ordinal,
                depth,
                key || value,
            )?;
            stack.last_mut().unwrap().ordinal += 1;
            if node.kind.container() {
                if stack.len() == 127 {
                    return Err("Search depth limit".into());
                }
                stack.push(Cursor {
                    parent: node,
                    ordinal: 0,
                    position: node.start + 1,
                });
            }
        }
        Ok(())
    }
}
