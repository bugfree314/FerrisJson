//! Windowed tree: disk-backed expansion exceptions and weighted row selection.
//! Physical windows contain real nodes only. No global row/expanded vector.
use super::{
    budget::{BudgetManager, Reservation, Resource},
    session::NodeRef,
    sparse::{Node, Page, SparseIndex, store::Store},
};
use crate::document::Document;
use std::{collections::VecDeque, sync::Arc};
const NONE: u64 = u64::MAX;
pub const WINDOW: usize = 64;
pub const MAX_DEPTH: usize = 128;
#[derive(Clone, Copy, Debug)]
enum Token {
    Legacy(usize),
    Sparse(Node),
}
#[derive(Clone, Copy, Debug)]
pub struct TreeNode {
    pub reference: NodeRef,
    pub kind: &'static str,
    pub children: u64,
    pub full_rows: u64,
    token: Token,
}
impl TreeNode {
    pub(crate) fn from_sparse(n: Node, full_rows: u64) -> Self {
        Self {
            reference: NodeRef {
                session: n.session(),
                address: n.start(),
            },
            kind: n.kind().name(),
            children: n.child_count(),
            full_rows,
            token: Token::Sparse(n),
        }
    }
    pub(crate) fn words(self) -> [u64; 8] {
        match self.token {
            Token::Legacy(id) => [
                0,
                self.reference.address,
                id as u64,
                kind_code(self.kind),
                0,
                0,
                self.children,
                self.full_rows,
            ],
            Token::Sparse(n) => {
                let w = n.words();
                [
                    1,
                    self.reference.address,
                    w[0],
                    w[1],
                    w[2],
                    w[3],
                    self.children,
                    self.full_rows,
                ]
            }
        }
    }
    pub(crate) fn restore(session: super::session::SessionId, w: [u64; 8]) -> Result<Self, String> {
        let (token, kind) = match w[0] {
            0 => (
                Token::Legacy(usize::try_from(w[2]).map_err(|_| "Saved ID overflow")?),
                kind_name(w[3])?,
            ),
            1 => {
                let n = Node::restore(session, [w[2], w[3], w[4], w[5], w[1]])?;
                (Token::Sparse(n), n.kind().name())
            }
            _ => return Err("Invalid result node tag".into()),
        };
        Ok(Self {
            reference: NodeRef {
                session,
                address: w[1],
            },
            kind,
            children: w[6],
            full_rows: w[7],
            token,
        })
    }
    pub fn container(self) -> bool {
        matches!(self.kind, "array" | "object")
    }
    pub fn sparse(self) -> Option<Node> {
        match self.token {
            Token::Sparse(n) => Some(n),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub parent: TreeNode,
    pub ordinal: u64,
}
#[derive(Clone, Debug)]
pub struct Location {
    pub node: TreeNode,
    pub ancestors: Vec<Edge>,
}
fn kind_code(kind: &str) -> u64 {
    match kind {
        "object" => 0,
        "array" => 1,
        "string" => 2,
        "number" => 3,
        "boolean" => 4,
        _ => 5,
    }
}
fn kind_name(n: u64) -> Result<&'static str, String> {
    match n {
        0 => Ok("object"),
        1 => Ok("array"),
        2 => Ok("string"),
        3 => Ok("number"),
        4 => Ok("boolean"),
        5 => Ok("null"),
        _ => Err("Invalid saved kind".into()),
    }
}
pub trait Provider: Send {
    fn query_field(
        &mut self,
        parent: TreeNode,
        name: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, TreeNode)>, String> {
        if parent.kind != "object" {
            return Ok(None);
        }
        for ordinal in 0..parent.children {
            let (key, truncated) = self.key(parent, ordinal, name.len() + 1, cancel)?;
            if !truncated && key == name {
                return Ok(Some((ordinal, self.child(parent, ordinal, cancel)?)));
            }
        }
        Ok(None)
    }
    fn search_input(&self) -> Result<super::search::Input, String> {
        Err("Search unavailable for this provider".into())
    }

    fn budget(&self) -> BudgetManager;
    fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String>;
    fn child(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<TreeNode, String>;
    fn prefix(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String>;
    fn key(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(String, bool), String>;
    fn preview(
        &mut self,
        node: TreeNode,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String>;
    fn complete(&self) -> bool {
        true
    }
    fn output(
        &mut self,
        node: TreeNode,
        cancel: &dyn Fn() -> bool,
    ) -> Result<OutputSelection, String>;
    fn weighted_child(
        &mut self,
        parent: TreeNode,
        rank: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, TreeNode, u64), String> {
        let (mut lo, mut hi) = (0, parent.children);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.prefix(parent, mid + 1, cancel)? > rank {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        if lo == parent.children {
            return Err("Weighted child out of range".into());
        }
        let inside = rank - self.prefix(parent, lo, cancel)?;
        Ok((lo, self.child(parent, lo, cancel)?, inside))
    }
    fn locate(&mut self, pointer: &str, cancel: &dyn Fn() -> bool) -> Result<Location, String> {
        if pointer.len() > 16 * 1024 {
            return Err("Pointer exceeds 16 KiB".into());
        }
        let mut location = Location {
            node: self.root(cancel)?,
            ancestors: Vec::new(),
        };
        if pointer.is_empty() {
            return Ok(location);
        }
        if !pointer.starts_with('/') {
            return Err("Expected JSON Pointer".into());
        }
        for raw in pointer[1..].split('/') {
            if location.ancestors.len() >= MAX_DEPTH {
                return Err("Tree depth limit".into());
            }
            let mut key = String::new();
            let mut chars = raw.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    key.push(match chars.next() {
                        Some('0') => '~',
                        Some('1') => '/',
                        _ => return Err("Invalid Pointer escape".into()),
                    });
                } else {
                    key.push(c);
                }
            }
            let parent = location.node;
            let ordinal = if parent.kind == "array" {
                if key.len() > 1 && key.starts_with('0') {
                    return Err("Invalid array ordinal".into());
                }
                key.parse::<u64>().map_err(|_| "Invalid array ordinal")?
            } else {
                return Err("Provider must implement object key lookup".into());
            };
            let node = self.child(parent, ordinal, cancel)?;
            location.ancestors.push(Edge { parent, ordinal });
            location.node = node;
        }
        Ok(location)
    }
    fn locate_ref(
        &mut self,
        _reference: NodeRef,
        _cancel: &dyn Fn() -> bool,
    ) -> Result<Location, String> {
        Err("Sparse result locations must carry ancestry".into())
    }
}
pub struct LegacyProvider(pub Arc<Document>);
impl LegacyProvider {
    pub(crate) fn node(&self, id: usize) -> TreeNode {
        Self::document_node(&self.0, id)
    }
    pub(crate) fn document_node(doc: &Document, id: usize) -> TreeNode {
        let n = doc.node_info(id);
        TreeNode {
            reference: n.reference,
            kind: n.kind,
            children: n.child_count.unwrap_or(0),
            full_rows: (n.subtree_end - id) as u64,
            token: Token::Legacy(id),
        }
    }
    fn id(&self, n: TreeNode) -> Result<usize, String> {
        let id = self.0.resolve(n.reference)?;
        if !matches!(n.token,Token::Legacy(i) if i==id) {
            return Err("Wrong legacy token".into());
        }
        Ok(id)
    }
}
impl Provider for LegacyProvider {
    fn query_field(
        &mut self,
        parent: TreeNode,
        name: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, TreeNode)>, String> {
        if parent.kind != "object" {
            return Ok(None);
        }
        let Some(id) = self.0.child_name(self.id(parent)?, name) else {
            return Ok(None);
        };
        let child = self.node(id);
        let (mut lo, mut hi) = (0, parent.children);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.child(parent, mid, cancel)?.reference.address < child.reference.address {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(Some((lo, child)))
    }
    fn search_input(&self) -> Result<super::search::Input, String> {
        Ok(super::search::Input::Legacy(self.0.clone()))
    }
    fn output(
        &mut self,
        node: TreeNode,
        cancel: &dyn Fn() -> bool,
    ) -> Result<OutputSelection, String> {
        check(cancel)?;
        Ok(OutputSelection::Legacy(self.0.clone(), self.id(node)?))
    }
    fn budget(&self) -> BudgetManager {
        self.0.budget().clone()
    }
    fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        check(cancel)?;
        Ok(self.node(0))
    }
    fn child(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<TreeNode, String> {
        check(cancel)?;
        self.id(parent)?;
        let page = self.0.children_page(parent.reference, ordinal, 1)?;
        let n = page.children.first().ok_or("Child out of range")?;
        Ok(self.node(n.reference.address as usize))
    }
    fn prefix(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        check(cancel)?;
        let id = self.id(parent)?;
        if ordinal > parent.children {
            return Err("Prefix out of range".into());
        }
        if ordinal == parent.children {
            return Ok(parent.full_rows - 1);
        }
        let child = self.child(parent, ordinal, cancel)?;
        Ok(child.reference.address - id as u64 - 1)
    }
    fn key(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(String, bool), String> {
        let child = self.child(parent, ordinal, cancel)?;
        let id = self.id(child)?;
        let key = self.0.key(id);
        let mut text = String::new();
        for c in key.chars() {
            if text.len() + c.len_utf8() > limit {
                return Ok((text, true));
            }
            text.push(c);
        }
        Ok((text, false))
    }
    fn preview(
        &mut self,
        node: TreeNode,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        check(cancel)?;
        let id = self.id(node)?;
        let mut text = if limit <= 640 {
            self.0.row_preview(id)
        } else {
            self.0.preview_ref(node.reference)?
        };
        trim(&mut text, limit);
        Ok(text)
    }
    fn locate(&mut self, path: &str, cancel: &dyn Fn() -> bool) -> Result<Location, String> {
        check(cancel)?;
        let reference = self.0.locate(if path.is_empty() { "$" } else { path })?;
        self.locate_ref(reference, cancel)
    }
    fn locate_ref(
        &mut self,
        reference: NodeRef,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Location, String> {
        let id = self.0.resolve(reference)?;
        let mut child = self.node(id);
        let mut edges = Vec::new();
        while let Some(parent_ref) = self.0.metadata(child.reference)?.parent {
            check(cancel)?;
            if edges.len() >= MAX_DEPTH {
                return Err("Tree depth limit".into());
            }
            let parent = self.node(parent_ref.address as usize);
            let (mut lo, mut hi) = (0, parent.children);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let n = self.child(parent, mid, cancel)?;
                if n.reference.address < child.reference.address {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if self.child(parent, lo, cancel)?.reference != child.reference {
                return Err("Ancestor ordinal mismatch".into());
            }
            edges.push(Edge {
                parent,
                ordinal: lo,
            });
            child = parent;
        }
        edges.reverse();
        Ok(Location {
            node: self.node(id),
            ancestors: edges,
        })
    }
}
pub struct SparseProvider {
    pub index: SparseIndex,
    pages: VecDeque<(u64, u64, Page)>,
    _cache: Reservation,
}
impl SparseProvider {
    pub fn new(index: SparseIndex) -> Result<Self, String> {
        let lease =
            index
                .budget()
                .reserve(Resource::Memory, 4096, "sparse tree page cache metadata")?;
        Ok(Self {
            index,
            pages: VecDeque::new(),
            _cache: lease,
        })
    }
    fn node(&self, n: Node, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        Ok(TreeNode {
            reference: NodeRef {
                session: self.index.session_id(),
                address: n.start(),
            },
            kind: n.kind().name(),
            children: n.child_count(),
            full_rows: self.index.full_rows(n, cancel)?,
            token: Token::Sparse(n),
        })
    }
    fn native(&self, n: TreeNode) -> Result<Node, String> {
        if n.reference.session != self.index.session_id() {
            return Err("Tree node belongs to another session".into());
        }
        n.sparse().ok_or("Wrong sparse token".into())
    }
}
impl Provider for SparseProvider {
    fn query_field(
        &mut self,
        parent: TreeNode,
        name: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, TreeNode)>, String> {
        if parent.kind != "object" {
            return Ok(None);
        }
        self.index
            .field_ordinal(self.native(parent)?, name, cancel)?
            .map(|(ordinal, n)| self.node(n, cancel).map(|node| (ordinal, node)))
            .transpose()
    }
    fn search_input(&self) -> Result<super::search::Input, String> {
        Ok(super::search::Input::Sparse(Box::new(
            self.index.read_view()?,
        )))
    }
    fn output(
        &mut self,
        node: TreeNode,
        cancel: &dyn Fn() -> bool,
    ) -> Result<OutputSelection, String> {
        Ok(OutputSelection::Sparse(
            self.index.output_selection(self.native(node)?, cancel)?,
        ))
    }
    fn budget(&self) -> BudgetManager {
        self.index.budget().clone()
    }
    fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        self.node(self.index.root(), cancel)
    }
    fn child(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<TreeNode, String> {
        let n = self.native(parent)?;
        let start = ordinal / 64 * 64;
        let key = parent.reference.address;
        if !self.pages.iter().any(|(p, s, _)| *p == key && *s == start) {
            let page = self.index.children(n, start, 64, cancel)?;
            if self.pages.len() == 8 {
                self.pages.pop_front();
            }
            self.pages.push_back((key, start, page));
        }
        let i = self
            .pages
            .iter()
            .position(|(p, s, _)| *p == key && *s == start)
            .unwrap();
        let entry = self.pages.remove(i).unwrap();
        let node = entry
            .2
            .nodes()
            .get((ordinal - start) as usize)
            .copied()
            .ok_or("Child out of range")?;
        self.pages.push_back(entry);
        self.node(node, cancel)
    }
    fn prefix(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        self.index
            .child_prefix(self.native(parent)?, ordinal, cancel)
    }
    fn weighted_child(
        &mut self,
        parent: TreeNode,
        rank: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, TreeNode, u64), String> {
        let (i, n, r) = self
            .index
            .weighted_child(self.native(parent)?, rank, cancel)?;
        Ok((i, self.node(n, cancel)?, r))
    }
    fn key(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(String, bool), String> {
        self.index
            .key_at(self.native(parent)?, ordinal, limit, cancel)
    }
    fn preview(
        &mut self,
        node: TreeNode,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        self.index.tree_preview(self.native(node)?, limit, cancel)
    }
    fn complete(&self) -> bool {
        self.index.complete()
    }
    fn locate(&mut self, pointer: &str, cancel: &dyn Fn() -> bool) -> Result<Location, String> {
        let path = self.index.pointer_edges(pointer, cancel)?;
        let mut location = Location {
            node: self.root(cancel)?,
            ancestors: Vec::new(),
        };
        for (ordinal, node) in path {
            let parent = location.node;
            location.node = self.node(node, cancel)?;
            location.ancestors.push(Edge { parent, ordinal });
        }
        Ok(location)
    }
}
pub struct TreeRow {
    pub location: Location,
    pub rank: u64,
    pub label: String,
    pub preview: String,
    pub expanded: bool,
}
pub enum OutputSelection {
    Legacy(Arc<Document>, usize),
    Sparse(super::sparse::RawSelection),
}
pub struct Clipboard {
    pub text: String,
    _lease: Reservation,
}
impl OutputSelection {
    pub fn write(
        &self,
        writer: &mut dyn std::io::Write,
        cancel: &dyn Fn() -> bool,
    ) -> serde_json::Result<()> {
        if cancel() {
            return Err(serde_json::Error::io(std::io::Error::other(
                "Export cancelled",
            )));
        }
        match self {
            Self::Legacy(doc, id) => doc.write_node(writer, *id),
            Self::Sparse(raw) => raw
                .write(writer, cancel)
                .map_err(|e| serde_json::Error::io(std::io::Error::other(e))),
        }
    }
    pub fn budget(&self) -> BudgetManager {
        match self {
            Self::Legacy(doc, _) => doc.budget().clone(),
            Self::Sparse(raw) => raw.budget(),
        }
    }
    pub fn copy(&self, cancel: &dyn Fn() -> bool) -> Result<Clipboard, String> {
        check(cancel)?;
        let mut lease = self.budget().reserve(
            Resource::Memory,
            crate::export::COPY_LIMIT,
            "tree clipboard transfer",
        )?;
        let text = match self {
            Self::Legacy(doc, id) => crate::export::Selection::Node(*id)
                .json(doc, Some(crate::export::COPY_LIMIT))
                .map_err(|e| format!("{e:?}"))?,
            Self::Sparse(raw) => raw.copy(cancel)?.text,
        };
        lease.resize(text.capacity())?;
        check(cancel)?;
        Ok(Clipboard {
            text,
            _lease: lease,
        })
    }
    pub fn export(
        &self,
        path: &std::path::Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), String> {
        crate::export::atomic_write_with_budget(path, cancel, &self.budget(), |writer| match self {
            Self::Legacy(doc, id) => doc.write_node(writer, *id),
            Self::Sparse(raw) => raw
                .write(writer, &|| {
                    cancel.load(std::sync::atomic::Ordering::Relaxed)
                })
                .map_err(|e| serde_json::Error::io(std::io::Error::other(e))),
        })
    }
}
pub struct TreeFrame {
    pub rows: Vec<TreeRow>,
    pub start: u64,
    pub total: u64,
    pub exact: bool,
    pub selected: Location,
    pub selected_rank: u64,
    pub selection: Arc<SelectionPreview>,
    _lease: Reservation,
}
pub struct SelectionPreview {
    pub path: String,
    pub preview: String,
    _lease: Reservation,
}
pub struct PagedTree {
    pub provider: Box<dyn Provider>,
    store: Store,
    states: u64,
    all: bool,
    _state: Reservation,
    invalid: bool,
    selection_cache: Option<(NodeRef, u64, u64, Arc<SelectionPreview>)>,
    edit_root: Option<u64>,
}
impl PagedTree {
    pub fn new(provider: Box<dyn Provider>, cancel: &dyn Fn() -> bool) -> Result<Self, String> {
        let budget = provider.budget();
        let state = budget.reserve(
            Resource::Memory,
            4 * 1024 * 1024,
            "paged tree bounded locations/history workspace",
        )?;
        let store = Store::new(&budget, cancel)?;
        let mut tree = Self {
            provider,
            store,
            states: NONE,
            all: false,
            _state: state,
            invalid: false,
            selection_cache: None,
            edit_root: None,
        };
        let root = tree.root(cancel)?;
        if root.node.container() {
            tree.set_expanded(&root, true, cancel)?;
        }
        Ok(tree)
    }
    pub fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<Location, String> {
        Ok(Location {
            node: self.provider.root(cancel)?,
            ancestors: Vec::new(),
        })
    }
    fn state(
        &self,
        node: TreeNode,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, [u64; 12])>, String> {
        let entry =
            self.store
                .directory_find(self.states, node.reference.address, false, cancel)?;
        if let Some(e) = entry.filter(|r| r[1] == node.reference.address) {
            let r = self.store.get(e[2], cancel)?;
            if r[0] != 5 || r[1] != node.reference.address {
                return Err("Invalid tree expansion record".into());
            }
            Ok(Some((e[2], r)))
        } else {
            Ok(None)
        }
    }
    pub fn expanded(&self, node: TreeNode, cancel: &dyn Fn() -> bool) -> Result<bool, String> {
        if !node.container() {
            return Ok(false);
        }
        Ok(self
            .state(node, cancel)?
            .map_or(self.all, |(_, r)| match r[3] {
                1 => false,
                2 => true,
                _ => self.all,
            }))
    }
    fn adjust(base: u64, actual: u64, basis: u64) -> Result<u64, String> {
        base.checked_add(actual)
            .and_then(|n| n.checked_sub(basis))
            .ok_or("Visible row count overflow".into())
    }
    fn calc(&self, node: TreeNode, r: [u64; 12], cancel: &dyn Fn() -> bool) -> Result<u64, String> {
        let expanded = match r[3] {
            1 => false,
            2 => true,
            _ => self.all,
        };
        if !expanded {
            return Ok(1);
        }
        let (a, b) = self.store.directory_sums(r[2], cancel)?;
        Self::adjust(
            if self.all {
                node.full_rows
            } else {
                1 + node.children
            },
            a,
            b,
        )
    }
    pub fn visible(&self, node: TreeNode, cancel: &dyn Fn() -> bool) -> Result<u64, String> {
        if let Some((_, r)) = self.state(node, cancel)? {
            self.calc(node, r, cancel)
        } else {
            Ok(if self.all { node.full_rows } else { 1 })
        }
    }
    pub fn total(&mut self, cancel: &dyn Fn() -> bool) -> Result<(u64, bool), String> {
        let root = self.provider.root(cancel)?;
        Ok((self.visible(root, cancel)?, self.provider.complete()))
    }
    fn ensure(
        &mut self,
        node: TreeNode,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, [u64; 12]), String> {
        if let Some(r) = self.state(node, cancel)? {
            return Ok(r);
        }
        let mut r = [0; 12];
        r[0] = 5;
        r[1] = node.reference.address;
        r[2] = NONE;
        r[4] = if self.all { node.full_rows } else { 1 };
        let id = self.store.append(r, cancel)?;
        self.states = self
            .store
            .directory_put(self.states, [r[1], id, 0, 0, 0], cancel)?;
        Ok((id, r))
    }
    pub fn normalize(
        &mut self,
        location: &Location,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Location, String> {
        if location.ancestors.len() >= MAX_DEPTH {
            return Err("Location exceeds tree depth budget".into());
        }
        let mut result = self.root(cancel)?;
        if result.node.reference.session != location.node.reference.session {
            return Err("Location belongs to another session".into());
        }
        for edge in &location.ancestors {
            let parent = result.node;
            result.node = self.provider.child(parent, edge.ordinal, cancel)?;
            result.ancestors.push(Edge {
                parent,
                ordinal: edge.ordinal,
            });
        }
        if result.node.reference != location.node.reference {
            return Err("Location identity changed".into());
        }
        Ok(result)
    }
    pub fn set_expanded(
        &mut self,
        location: &Location,
        expanded: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        if self.invalid {
            return Err("Tree state invalid; reset required".into());
        }
        let location = self.normalize(location, cancel)?;
        if !location.node.container() {
            return Ok(());
        }
        let old = self.states;
        let budget = self.provider.budget();
        let own_transaction = !self.store.in_transaction();
        if own_transaction {
            self.store.transaction(&budget)?;
        }
        let result = (|| {
            let (id, mut r) = self.ensure(location.node, cancel)?;
            r[3] = if expanded { 2 } else { 1 };
            let mut weight = self.calc(location.node, r, cancel)?;
            r[4] = weight;
            self.store.put(id, r, cancel)?;
            let mut child = location.node;
            for edge in location.ancestors.iter().rev() {
                check(cancel)?;
                let (parent_id, mut state) = self.ensure(edge.parent, cancel)?;
                state[2] = self.store.directory_put(
                    state[2],
                    [
                        edge.ordinal,
                        child.reference.address,
                        weight,
                        0,
                        if self.all { child.full_rows } else { 1 },
                    ],
                    cancel,
                )?;
                weight = self.calc(edge.parent, state, cancel)?;
                state[4] = weight;
                self.store.put(parent_id, state, cancel)?;
                child = edge.parent;
            }
            Ok(())
        })();
        if !own_transaction {
            return result;
        }
        if result.is_err() {
            self.states = old;
            if let Err(e) = self.store.finish_transaction(false) {
                self.invalid = true;
                return Err(format!("Tree rollback failed: {e}"));
            }
        } else {
            self.store.finish_transaction(true)?;
        }
        result
    }
    pub fn begin_edit(&mut self) -> Result<(), String> {
        if self.edit_root.is_some() {
            return Err("Nested navigation edit".into());
        }
        self.store.transaction(&self.provider.budget())?;
        self.edit_root = Some(self.states);
        Ok(())
    }
    pub fn finish_edit(&mut self, commit: bool) -> Result<(), String> {
        let root = self.edit_root.take().ok_or("No navigation edit")?;
        if !commit {
            self.states = root;
        }
        if let Err(e) = self.store.finish_transaction(commit) {
            self.invalid = true;
            return Err(e);
        }
        Ok(())
    }
    pub fn reset(&mut self, all: bool, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        let fresh = Store::new(&self.provider.budget(), cancel)?;
        self.store = fresh;
        self.states = NONE;
        self.all = all;
        self.invalid = false;
        Ok(())
    }
    fn prefix(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        let base = if self.all {
            self.provider.prefix(parent, ordinal, cancel)?
        } else {
            ordinal
        };
        let (a, b) = if let Some((_, r)) = self.state(parent, cancel)? {
            self.store.directory_prefix(r[2], ordinal, cancel)?
        } else {
            (0, 0)
        };
        Self::adjust(base, a, b)
    }
    pub fn row(&mut self, mut rank: u64, cancel: &dyn Fn() -> bool) -> Result<Location, String> {
        let mut location = self.root(cancel)?;
        if rank >= self.visible(location.node, cancel)? {
            return Err(if self.provider.complete() {
                "Row out of range"
            } else {
                "Pending: row not committed"
            }
            .into());
        }
        for _ in 0..MAX_DEPTH {
            check(cancel)?;
            if rank == 0 {
                return Ok(location);
            }
            let parent = location.node;
            if !self.expanded(parent, cancel)? {
                return Err("Row descends into collapsed node".into());
            }
            rank -= 1;
            let no_deltas = self
                .state(parent, cancel)?
                .is_none_or(|(_, r)| r[2] == NONE);
            let (ordinal, node, inside) = if self.all && no_deltas {
                self.provider.weighted_child(parent, rank, cancel)?
            } else if !self.all && no_deltas {
                (rank, self.provider.child(parent, rank, cancel)?, 0)
            } else {
                let (mut lo, mut hi) = (0, parent.children);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if self.prefix(parent, mid + 1, cancel)? > rank {
                        hi = mid;
                    } else {
                        lo = mid + 1;
                    }
                }
                if lo == parent.children {
                    return Err("Visible child rank out of range".into());
                }
                let inside = rank - self.prefix(parent, lo, cancel)?;
                (lo, self.provider.child(parent, lo, cancel)?, inside)
            };
            location.ancestors.push(Edge { parent, ordinal });
            location.node = node;
            rank = inside;
        }
        Err("Tree depth limit".into())
    }
    pub fn rank(&mut self, location: &Location, cancel: &dyn Fn() -> bool) -> Result<u64, String> {
        let location = self.normalize(location, cancel)?;
        let mut rank = 0u64;
        for edge in &location.ancestors {
            if !self.expanded(edge.parent, cancel)? {
                return Err("Location is hidden by a collapsed ancestor".into());
            }
            rank = rank
                .checked_add(1 + self.prefix(edge.parent, edge.ordinal, cancel)?)
                .ok_or("Rank overflow")?;
        }
        Ok(rank)
    }
    pub fn reveal(
        &mut self,
        location: &Location,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Location, String> {
        let location = self.normalize(location, cancel)?;
        for depth in 0..location.ancestors.len() {
            let parent = Location {
                node: location.ancestors[depth].parent,
                ancestors: location.ancestors[..depth].to_vec(),
            };
            if !self.expanded(parent.node, cancel)? {
                self.set_expanded(&parent, true, cancel)?;
            }
        }
        Ok(location)
    }
    pub fn reveal_ref(
        &mut self,
        reference: NodeRef,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Location, String> {
        let location = self.provider.locate_ref(reference, cancel)?;
        self.reveal(&location, cancel)
    }
    fn next(
        &mut self,
        location: &Location,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<Location>, String> {
        let mut next = location.clone();
        if self.expanded(next.node, cancel)? && next.node.children > 0 {
            let parent = next.node;
            next.node = self.provider.child(parent, 0, cancel)?;
            next.ancestors.push(Edge { parent, ordinal: 0 });
            return Ok(Some(next));
        }
        while let Some(edge) = next.ancestors.pop() {
            if edge.ordinal + 1 < edge.parent.children {
                next.node = self.provider.child(edge.parent, edge.ordinal + 1, cancel)?;
                next.ancestors.push(Edge {
                    parent: edge.parent,
                    ordinal: edge.ordinal + 1,
                });
                return Ok(Some(next));
            }
        }
        Ok(None)
    }
    pub fn path(
        &mut self,
        location: &Location,
        limit: usize,
        full: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        let location = self.normalize(location, cancel)?;
        let mut path = String::from("$");
        for edge in &location.ancestors {
            check(cancel)?;
            let remaining = limit.saturating_sub(path.len());
            let (key, truncated) =
                self.provider
                    .key(edge.parent, edge.ordinal, remaining, cancel)?;
            let segment = if edge.parent.kind == "array" {
                format!("[{}]", edge.ordinal)
            } else {
                format!(
                    "[{}]",
                    serde_json::to_string(&key).map_err(|e| e.to_string())?
                )
            };
            if truncated || path.len() + segment.len() > limit {
                if full {
                    return Err("Full path exceeds copy budget".into());
                }
                path.push('…');
                break;
            }
            path.push_str(&segment);
        }
        Ok(path)
    }
    pub fn frame(
        &mut self,
        start: u64,
        limit: usize,
        selected: &Location,
        cancel: &dyn Fn() -> bool,
    ) -> Result<TreeFrame, String> {
        if limit == 0 || limit > 256 {
            return Err("Tree window must contain 1..256 rows".into());
        }
        let budget = self.provider.budget();
        let max = limit
            * (MAX_DEPTH * std::mem::size_of::<Edge>() + std::mem::size_of::<TreeRow>() + 1024)
            + 512 * 1024;
        let lease = budget.reserve(
            Resource::Memory,
            max,
            "bounded tree frame and selection payload",
        )?;
        let (total, exact) = self.total(cancel)?;
        let start = start.min(total.saturating_sub(1));
        let mut location = self.row(start, cancel)?;
        let mut rows = Vec::with_capacity(limit);
        for offset in 0..limit {
            check(cancel)?;
            let label = if let Some(edge) = location.ancestors.last() {
                let (mut key, cut) = self.provider.key(edge.parent, edge.ordinal, 640, cancel)?;
                if cut {
                    key.push('…');
                }
                key
            } else {
                "$".into()
            };
            let preview = self.provider.preview(location.node, 160, cancel)?;
            let expanded = self.expanded(location.node, cancel)?;
            rows.push(TreeRow {
                location: location.clone(),
                rank: start + offset as u64,
                label,
                preview,
                expanded,
            });
            if offset + 1 == limit {
                break;
            }
            match self.next(&location, cancel)? {
                Some(n) => location = n,
                None => break,
            }
        }
        let selected = self.normalize(selected, cancel)?;
        let selected_rank = self.rank(&selected, cancel)?;
        let selection = if let Some((reference, children, full_rows, preview)) =
            &self.selection_cache
            && *reference == selected.node.reference
            && *children == selected.node.children
            && *full_rows == selected.node.full_rows
        {
            preview.clone()
        } else {
            let lease = budget.reserve(
                Resource::Memory,
                512 * 1024,
                "cached selected path/preview payload",
            )?;
            let path = self.path(&selected, 16 * 1024, false, cancel)?;
            let preview = self.provider.preview(selected.node, 200_000, cancel)?;
            let selection = Arc::new(SelectionPreview {
                path,
                preview,
                _lease: lease,
            });
            self.selection_cache = Some((
                selected.node.reference,
                selected.node.children,
                selected.node.full_rows,
                selection.clone(),
            ));
            selection
        };
        Ok(TreeFrame {
            rows,
            start,
            total,
            exact,
            selected,
            selected_rank,
            selection,
            _lease: lease,
        })
    }
    pub fn state_bytes(&self) -> u64 {
        self.store.bytes()
    }
}
fn check(cancel: &dyn Fn() -> bool) -> Result<(), String> {
    if cancel() {
        Err("Tree operation cancelled".into())
    } else {
        super::file_source::check_cancelled().map_err(|e| e.to_string())
    }
}
fn trim(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut n = limit;
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        text.truncate(n);
        text.push('…');
    }
}
#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
