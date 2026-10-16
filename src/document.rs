use serde_json::Value;
use serde_json_path::{JsonPath, PathElement};
use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Write},
    sync::Arc,
};
#[path = "indexed.rs"]
pub(crate) mod indexed;
pub use indexed::LoadProgress;
pub use indexed::Subtree;

pub use crate::engine::ids::NodeIds;
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource},
    session::{ChildPage, NodeInfo, NodeRef, SessionId, SessionStatus},
};

#[derive(Clone, Copy)]
enum Kind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}
impl Kind {
    fn of(value: &Value) -> Self {
        match value {
            Value::Object(_) => Self::Object,
            Value::Array(_) => Self::Array,
            Value::String(_) => Self::String,
            Value::Number(_) => Self::Number,
            Value::Bool(_) => Self::Boolean,
            Value::Null => Self::Null,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Null => "null",
        }
    }
}

// Preorder node IDs remain stable. Strings and complete paths are never stored per node.
pub struct Node {
    parent: u32,
    pub depth: u32,
    end: u32,
    segment: u32,
    child_start: u32,
    child_count: u32,
    kind: Kind,
}
impl Node {
    pub fn parent(&self) -> Option<usize> {
        (self.parent != u32::MAX).then_some(self.parent as usize)
    }
    pub fn end(&self) -> usize {
        self.end as usize
    }
    pub fn kind(&self) -> &'static str {
        self.kind.name()
    }
    pub fn container(&self) -> bool {
        matches!(self.kind, Kind::Object | Kind::Array)
    }
}
pub struct Document {
    storage: Storage,
    session: SessionId,
    budget: BudgetManager,
    nodes: Vec<Node>,
    pub bytes: usize,
    children: Vec<u32>,
    keys: Vec<Arc<str>>,
    key_ids: HashMap<Arc<str>, u32>,
    _resources: Vec<Reservation>,
}
enum Storage {
    Memory(Value),
    Indexed(indexed::Store),
}
struct IndexBuilder {
    nodes: Vec<Node>,
    children: Vec<u32>,
    keys: Vec<Arc<str>>,
    key_ids: HashMap<Arc<str>, u32>,
    resources: Vec<Reservation>,
}
impl IndexBuilder {
    fn index(
        &mut self,
        value: &Value,
        segment: u32,
        parent: u32,
        depth: u32,
    ) -> Result<u32, String> {
        let id = self.nodes.len();
        let count = match value {
            Value::Object(v) => v.len(),
            Value::Array(v) => v.len(),
            _ => 0,
        };
        let start = self.children.len();
        self.children.resize(start + count, 0);
        self.nodes.push(Node {
            parent,
            depth,
            end: compact(id + 1)?,
            segment,
            child_start: compact(start)?,
            child_count: compact(count)?,
            kind: Kind::of(value),
        });
        match value {
            Value::Object(map) => {
                for (ordinal, (key, child)) in map.iter().enumerate() {
                    let segment = if let Some(&index) = self.key_ids.get(key.as_str()) {
                        index
                    } else {
                        let index = compact(self.keys.len())?;
                        let extra = key.len() + 2 * std::mem::size_of::<Arc<str>>() + 192;
                        let lease = &mut self.resources[2];
                        lease.resize(
                            lease
                                .bytes()
                                .checked_add(extra)
                                .ok_or("Key size overflow")?,
                        )?;
                        let key: Arc<str> = Arc::from(key.as_str());
                        self.key_ids.insert(Arc::clone(&key), index);
                        self.keys.push(key);
                        index
                    };
                    let child_id = self.index(child, segment, compact(id)?, depth + 1)?;
                    self.children[start + ordinal] = child_id;
                }
                // Lookup order is independent of display order, which stays in preorder.
                self.children[start..start + count]
                    .sort_unstable_by_key(|&child| self.nodes[child as usize].segment);
            }
            Value::Array(array) => {
                for (ordinal, child) in array.iter().enumerate() {
                    let child_id = self.index(child, compact(ordinal)?, compact(id)?, depth + 1)?;
                    self.children[start + ordinal] = child_id;
                }
            }
            _ => (),
        }
        self.nodes[id].end = compact(self.nodes.len())?;
        compact(id)
    }
}
fn compact(value: usize) -> Result<u32, String> {
    u32::try_from(value)
        .map_err(|_| "Document exceeds the 32-bit index limit (4,294,967,295 nodes)".into())
}
fn node_count(value: &Value) -> Result<usize, String> {
    let add = |count: usize, child: &Value| {
        let total = count
            .checked_add(node_count(child)?)
            .ok_or("Node count overflow")?;
        compact(total)?;
        Ok(total)
    };
    match value {
        Value::Object(v) => v.values().try_fold(1, add),
        Value::Array(v) => v.iter().try_fold(1, add),
        _ => Ok(1),
    }
}
impl Document {
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_owned(text.to_owned())
    }
    /// Consume input so its allocation is released before the full index is built.
    pub fn parse_owned(text: String) -> Result<Self, String> {
        Self::parse_owned_recover(text).map_err(|(error, _)| error)
    }
    /// Return the original allocation on syntax errors so the paste editor can repair it.
    pub fn parse_owned_recover(text: String) -> Result<Self, (String, Option<String>)> {
        Self::parse_owned_recover_with_budget(text, BudgetManager::default())
    }
    pub fn parse_owned_recover_with_budget(
        text: String,
        budget: BudgetManager,
    ) -> Result<Self, (String, Option<String>)> {
        let input = match budget.reserve(Resource::Memory, text.capacity(), "input text") {
            Ok(lease) => lease,
            Err(error) => return Err((error, Some(text))),
        };
        let bytes = text.len();
        let value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(value) => value,
            Err(error) => return Err((format!("Invalid JSON: {error}"), Some(text))),
        };
        drop(text);
        drop(input);
        Self::from_value_with_budget(value, bytes, budget).map_err(|error| (error, None))
    }
    pub fn from_value(value: Value, bytes: usize) -> Result<Self, String> {
        Self::from_value_with_budget(value, bytes, BudgetManager::default())
    }
    pub fn from_value_with_budget(
        value: Value,
        bytes: usize,
        budget: BudgetManager,
    ) -> Result<Self, String> {
        // Counting avoids geometric capacity growth for the two large index arrays.
        let count = node_count(&value)?;
        let node_bytes = count
            .checked_mul(std::mem::size_of::<Node>())
            .ok_or("Index size overflow")?;
        let child_bytes = count
            .saturating_sub(1)
            .checked_mul(4)
            .ok_or("Index size overflow")?;
        let mut resources = vec![
            budget.reserve(
                Resource::Memory,
                node_bytes + child_bytes,
                "memory structure",
            )?,
            // serde builds Value internally; this is a conservative retained estimate,
            // not enforcement of the parser's transient allocations.
            budget.reserve(
                Resource::Memory,
                indexed::value_cost(&value),
                "retained Value estimate",
            )?,
        ];
        resources.push(budget.reserve(Resource::Memory, 0, "shared key estimate")?);
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|e| format!("Cannot allocate nodes: {e}"))?;
        let mut children = Vec::new();
        children
            .try_reserve_exact(count.saturating_sub(1))
            .map_err(|e| format!("Cannot allocate children: {e}"))?;
        resources[0]
            .resize(nodes.capacity() * std::mem::size_of::<Node>() + children.capacity() * 4)?;
        let mut index = IndexBuilder {
            nodes,
            children,
            keys: Vec::new(),
            key_ids: HashMap::new(),
            resources,
        };
        index.index(&value, 0, u32::MAX, 0)?;
        Ok(Self {
            storage: Storage::Memory(value),
            session: SessionId::fresh(),
            budget,
            _resources: index.resources,
            nodes: index.nodes,
            bytes,
            children: index.children,
            keys: index.keys,
            key_ids: index.key_ids,
        })
    }
    #[allow(dead_code, reason = "Used by the standalone memory benchmark")]
    pub fn structure_capacity_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.children.capacity() * std::mem::size_of::<u32>()
            + match &self.storage {
                Storage::Indexed(store) => store.range_bytes(),
                _ => 0,
            }
    }
    pub fn open_indexed(
        path: &std::path::Path,
        cancelled: impl Fn() -> bool,
        progress: &LoadProgress,
    ) -> Result<Self, String> {
        Self::open_indexed_with_budget(path, cancelled, progress, BudgetManager::default())
    }
    pub fn open_indexed_with_budget(
        path: &std::path::Path,
        cancelled: impl Fn() -> bool,
        progress: &LoadProgress,
        budget: BudgetManager,
    ) -> Result<Self, String> {
        indexed::open_with_budget(path, cancelled, progress, budget)
    }
    pub fn budget(&self) -> &BudgetManager {
        &self.budget
    }
    pub fn session_id(&self) -> SessionId {
        self.session
    }
    pub fn status(&self) -> SessionStatus {
        SessionStatus::Ready
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn node_capacity_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
    }
    pub fn node_ref(&self, id: usize) -> Result<NodeRef, String> {
        if id >= self.nodes.len() {
            return Err("Invalid node ID".into());
        }
        Ok(NodeRef {
            session: self.session,
            address: id as u64,
        })
    }
    pub fn resolve(&self, reference: NodeRef) -> Result<usize, String> {
        if reference.session != self.session {
            return Err("Node belongs to a different document session".into());
        }
        let id = usize::try_from(reference.address).map_err(|_| "Invalid node address")?;
        if id >= self.nodes.len() {
            return Err("Invalid node address".into());
        }
        Ok(id)
    }
    /// Complete-index adapter metadata. No borrowed storage nodes escape the engine.
    pub fn node_info(&self, id: usize) -> NodeInfo {
        let node = &self.nodes[id];
        NodeInfo {
            reference: NodeRef {
                session: self.session,
                address: id as u64,
            },
            parent: node.parent().map(|parent| NodeRef {
                session: self.session,
                address: parent as u64,
            }),
            depth: node.depth,
            kind: node.kind(),
            child_count: Some(node.child_count as u64),
            subtree_end: node.end(),
        }
    }
    pub fn metadata(&self, reference: NodeRef) -> Result<NodeInfo, String> {
        Ok(self.node_info(self.resolve(reference)?))
    }
    pub fn locate(&self, path: &str) -> Result<NodeRef, String> {
        self.node_ref(self.find_path(path)?)
    }
    pub fn preview_ref(&self, reference: NodeRef) -> Result<String, String> {
        let id = self.resolve(reference)?;
        crate::engine::file_source::check_cancelled().map_err(|e| e.to_string())?;
        let preview = self.preview(id);
        crate::engine::file_source::check_cancelled().map_err(|e| e.to_string())?;
        Ok(preview)
    }
    pub fn path_ref(&self, reference: NodeRef) -> Result<String, String> {
        Ok(self.path(self.resolve(reference)?))
    }
    /// A bounded display string; copying/exporting paths still uses path_ref.
    pub fn display_path_ref(&self, reference: NodeRef) -> Result<String, String> {
        let id = self.resolve(reference)?;
        const LIMIT: usize = 16 * 1024;
        let _lease = self
            .budget
            .reserve(Resource::Memory, LIMIT + 3, "display path")?;
        struct Buffer {
            bytes: Vec<u8>,
            truncated: bool,
        }
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                crate::engine::file_source::check_cancelled()?;
                let remaining = LIMIT.saturating_sub(self.bytes.len());
                self.bytes
                    .extend_from_slice(&bytes[..remaining.min(bytes.len())]);
                if bytes.len() > remaining {
                    self.truncated = true;
                    return Err(io::Error::other("display path limit"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Buffer {
            bytes: Vec::with_capacity(LIMIT + 3),
            truncated: false,
        };
        buffer.write_all(b"$").map_err(|e| e.to_string())?;
        for child in self.ancestors(id) {
            let node = &self.nodes[child];
            let result = if matches!(self.nodes[node.parent().unwrap()].kind, Kind::Array) {
                write!(&mut buffer, "[{}]", node.segment)
            } else {
                let key = &self.keys[node.segment as usize];
                let mut characters = key.chars();
                let mut identifier = characters
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
                if identifier {
                    for (index, character) in characters.enumerate() {
                        if index % 1024 == 0 {
                            crate::engine::file_source::check_cancelled()
                                .map_err(|e| e.to_string())?;
                        }
                        if !character.is_ascii_alphanumeric() && character != '_' {
                            identifier = false;
                            break;
                        }
                    }
                }
                if identifier {
                    buffer
                        .write_all(b".")
                        .and_then(|_| buffer.write_all(key.as_bytes()))
                } else {
                    buffer
                        .write_all(b"[")
                        .and_then(|_| write_escaped_preview_string(&mut buffer, key))
                        .and_then(|_| buffer.write_all(b"]"))
                }
            };
            if let Err(error) = result {
                if !buffer.truncated {
                    return Err(error.to_string());
                }
                break;
            }
        }
        let valid = std::str::from_utf8(&buffer.bytes)
            .map_or_else(|e| e.valid_up_to(), |_| buffer.bytes.len());
        buffer.bytes.truncate(valid);
        let mut path = String::from_utf8(buffer.bytes).unwrap();
        if buffer.truncated {
            path.push('…');
        }
        Ok(path)
    }
    /// Pages preserve source/display order, not the internal sorted lookup table.
    pub fn children_page(
        &self,
        reference: NodeRef,
        offset: u64,
        limit: usize,
    ) -> Result<ChildPage, String> {
        let id = self.resolve(reference)?;
        if limit == 0 || limit > 4096 {
            return Err("Child page limit must be between 1 and 4096".into());
        }
        let count = self.nodes[id].child_count as u64;
        if offset > count {
            return Err("Child page offset exceeds child count".into());
        }
        let take = limit.min((count - offset) as usize);
        let reservation = self.budget.reserve(
            Resource::Memory,
            take * std::mem::size_of::<NodeInfo>(),
            "child page",
        )?;
        let children = self
            .direct_children(id)
            .skip(offset as usize)
            .take(take)
            .map(|child| self.node_info(child))
            .collect();
        let next = offset + take as u64;
        Ok(ChildPage {
            children,
            next: (next < count).then_some(next),
            total: Some(count),
            _reservation: reservation,
        })
    }
    pub fn backend_name(&self) -> &'static str {
        match self.storage {
            Storage::Memory(_) => "In-memory",
            Storage::Indexed(_) => "Range index",
        }
    }
    #[allow(
        dead_code,
        reason = "Memory backend inspection for tests and baseline benchmarks"
    )]
    pub fn value(&self) -> Option<&Value> {
        match &self.storage {
            Storage::Memory(value) => Some(value),
            _ => None,
        }
    }
    #[allow(
        dead_code,
        reason = "Explicit bounded subtree parsing interface for indexed backend"
    )]
    pub fn read_subtree(&self, id: usize) -> Result<Arc<indexed::Subtree>, String> {
        match &self.storage {
            Storage::Indexed(store) => store.subtree(self, id),
            _ => Err("Bounded subtree cache is an indexed-backend interface".into()),
        }
    }
    #[cfg(test)]
    pub fn at(&self, id: usize) -> &Value {
        self.memory_at(id)
    }
    fn memory_at(&self, id: usize) -> &Value {
        let node = &self.nodes[id];
        let Some(parent) = node.parent() else {
            return self
                .value()
                .expect("Value references only exist in the in-memory backend");
        };
        match self.memory_at(parent) {
            Value::Object(map) => &map[self.keys[node.segment as usize].as_ref()],
            Value::Array(array) => &array[node.segment as usize],
            _ => unreachable!("only containers have children"),
        }
    }
    pub fn key(&self, id: usize) -> Cow<'_, str> {
        let node = &self.nodes[id];
        match node.parent() {
            None => Cow::Borrowed("$"),
            Some(parent) if matches!(self.nodes[parent].kind, Kind::Array) => {
                Cow::Owned(node.segment.to_string())
            }
            Some(_) => Cow::Borrowed(&self.keys[node.segment as usize]),
        }
    }
    fn ancestors(&self, mut id: usize) -> Vec<usize> {
        let mut result = Vec::with_capacity(self.nodes[id].depth as usize);
        while let Some(parent) = self.nodes[id].parent() {
            result.push(id);
            id = parent;
        }
        result.reverse();
        result
    }
    pub fn path(&self, id: usize) -> String {
        let mut path = String::from("$");
        for child in self.ancestors(id) {
            let node = &self.nodes[child];
            if matches!(self.nodes[node.parent().unwrap()].kind, Kind::Array) {
                path.push_str(&format!("[{}]", node.segment));
            } else {
                let key = &self.keys[node.segment as usize];
                if is_identifier(key) {
                    path.push('.');
                    path.push_str(key);
                } else {
                    path.push('[');
                    path.push_str(&serde_json::to_string(key.as_ref()).unwrap());
                    path.push(']');
                }
            }
        }
        path
    }
    #[cfg(test)]
    fn pointer(&self, id: usize) -> String {
        let mut pointer = String::new();
        for child in self.ancestors(id) {
            pointer.push('/');
            pointer.push_str(&self.key(child).replace('~', "~0").replace('/', "~1"));
        }
        pointer
    }
    fn child_name(&self, parent: usize, name: &str) -> Option<usize> {
        if !matches!(self.nodes[parent].kind, Kind::Object) {
            return None;
        }
        let &segment = self.key_ids.get(name)?;
        let node = &self.nodes[parent];
        let children = &self.children
            [node.child_start as usize..(node.child_start as usize + node.child_count as usize)];
        children
            .binary_search_by_key(&segment, |&id| self.nodes[id as usize].segment)
            .ok()
            .map(|i| children[i] as usize)
    }
    fn child_index(&self, parent: usize, index: usize) -> Option<usize> {
        let node = &self.nodes[parent];
        (matches!(node.kind, Kind::Array) && index < node.child_count as usize)
            .then(|| self.children[node.child_start as usize + index] as usize)
    }
    fn pointer_id(&self, pointer: &str) -> Option<usize> {
        let mut id = 0;
        for token in pointer.strip_prefix('/')?.split('/') {
            let mut bytes = token.bytes();
            while let Some(byte) = bytes.next() {
                if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
                    return None;
                }
            }
            let token = token.replace("~1", "/").replace("~0", "~");
            id = if matches!(self.nodes[id].kind, Kind::Array) {
                if token.starts_with('+') || (token.len() > 1 && token.starts_with('0')) {
                    return None;
                }
                self.child_index(id, token.parse().ok()?)?
            } else {
                self.child_name(id, &token)?
            };
        }
        Some(id)
    }
    pub fn find_path(&self, path: &str) -> Result<usize, String> {
        let ids = self.query_path(path)?;
        match ids.as_slice() {
            [id] => Ok(*id as usize),
            [] => Err(format!("Path not found: {path}")),
            _ => Err("Path selected multiple nodes; use --query to retrieve all results".into()),
        }
    }
    pub fn query_path(&self, expression: &str) -> Result<NodeIds, String> {
        self.query_path_cancellable(expression, || false)
    }
    pub fn query_path_cancellable(
        &self,
        expression: &str,
        cancelled: impl Fn() -> bool,
    ) -> Result<NodeIds, String> {
        if cancelled() {
            return Err("Query cancelled".into());
        }
        let expression = expression.trim();
        if expression.starts_with('/') {
            let mut ids = NodeIds::new(&self.budget);
            if let Some(id) = self.pointer_id(expression) {
                ids.push(id as u32)?;
            }
            return Ok(ids);
        }
        let normalized = if expression == "." {
            "$".into()
        } else if expression.starts_with('.') || expression.starts_with('[') {
            format!("${expression}")
        } else {
            expression.to_owned()
        };
        let query = JsonPath::parse(&normalized).map_err(|e| format!("Invalid JSONPath: {e}"))?;
        if matches!(self.storage, Storage::Indexed(_)) {
            return indexed::query(self, &normalized, cancelled);
        }
        let results = query.query_located(self.value().unwrap());
        if cancelled() {
            return Err("Query cancelled".into());
        }
        let mut ids = NodeIds::new(&self.budget);
        ids.reserve_exact(results.len())?;
        for (index, node) in results.iter().enumerate() {
            if index % 1024 == 0 && cancelled() {
                return Err("Query cancelled".into());
            }
            let mut id = 0;
            for element in node.location().iter() {
                id = match element {
                    PathElement::Name(name) => self.child_name(id, name),
                    PathElement::Index(index) => self.child_index(id, *index),
                }
                .ok_or_else(|| "Query result not found in tree".to_string())?;
            }
            ids.push(id as u32)?;
        }
        Ok(ids)
    }

    pub fn row_preview(&self, id: usize) -> String {
        if let Storage::Indexed(store) = &self.storage {
            return store
                .row_preview(self, id)
                .unwrap_or_else(|error| format!("Read error: {error}"));
        }
        match self.memory_at(id) {
            Value::Object(v) => format!("{{ {} keys }}", v.len()),
            Value::Array(v) => format!("[ {} items ]", v.len()),
            Value::String(v) => serde_json::to_string(&short(v, 160)).unwrap(),
            Value::Number(v) => short(v.as_str(), 160),
            value => value.to_string(),
        }
    }
    pub fn preview(&self, id: usize) -> String {
        if let Storage::Indexed(store) = &self.storage
            && let Ok(value) = store.subtree(self, id)
        {
            return bounded_preview(&self.budget, |writer| {
                serde_json::to_writer_pretty(writer, value.as_ref())
            });
        }
        bounded_preview(&self.budget, |writer| match &self.storage {
            Storage::Memory(_) => {
                write_memory_preview(writer, self.memory_at(id), 0).map_err(serde_json::Error::io)
            }
            Storage::Indexed(_) => self.write_node(writer, id),
        })
    }
    #[cfg(test)]
    pub fn results_json(&self, ids: &[u32]) -> String {
        let mut bytes = Vec::new();
        self.write_results(&mut bytes, ids).unwrap();
        String::from_utf8(bytes).unwrap()
    }
    pub fn results_preview(&self, ids: &[u32]) -> String {
        bounded_preview(&self.budget, |writer| {
            if matches!(self.storage, Storage::Memory(_)) {
                (|| {
                    writer.write_all(b"[")?;
                    for (ordinal, &id) in ids.iter().enumerate() {
                        writer.write_all(if ordinal == 0 { b"\n  " } else { b",\n  " })?;
                        write_memory_preview(writer, self.memory_at(id as usize), 1)?;
                    }
                    if !ids.is_empty() {
                        writer.write_all(b"\n")?;
                    }
                    writer.write_all(b"]")
                })()
                .map_err(serde_json::Error::io)
            } else {
                self.write_results(writer, ids)
            }
        })
    }
    pub fn write_node(&self, mut writer: impl Write, id: usize) -> serde_json::Result<()> {
        match &self.storage {
            Storage::Memory(_) => serde_json::to_writer_pretty(writer, self.memory_at(id)),
            Storage::Indexed(store) => store
                .write(self, &mut writer, &[id as u32], false)
                .map_err(serde_json::Error::io),
        }
    }
    pub fn write_results(&self, mut writer: impl Write, ids: &[u32]) -> serde_json::Result<()> {
        match &self.storage {
            Storage::Memory(_) => {
                serde_json::to_writer_pretty(writer, &SelectedValues { doc: self, ids })
            }
            Storage::Indexed(store) => store
                .write(self, &mut writer, ids, true)
                .map_err(serde_json::Error::io),
        }
    }
    pub fn search(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
    ) -> Result<NodeIds, String> {
        self.search_cancellable(term, case, regex, scope, || false)
    }
    pub(crate) fn search_visit(
        &self,
        matcher: &crate::engine::search::Matcher,
        cancel: &dyn Fn() -> bool,
        visit: &mut dyn FnMut(usize, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        if let Storage::Indexed(store) = &self.storage {
            return store.search_visit(self, matcher, cancel, visit);
        }
        let mut id = 0;
        let mut error = None;
        walk(self.value().unwrap(), &mut |value| {
            if cancel() {
                error = Some("Search cancelled".into());
                return false;
            }
            let key = matcher.keys() && matcher.text(&self.key(id));
            let matched = key
                || (matcher.values()
                    && match value {
                        Value::String(s) => matcher.text(s),
                        Value::Number(n) => matcher.text(n.as_str()),
                        Value::Bool(b) => matcher.text(if *b { "true" } else { "false" }),
                        Value::Null => matcher.text("null"),
                        _ => false,
                    });
            match visit(id, matched) {
                Ok(()) => {
                    id += 1;
                    true
                }
                Err(e) => {
                    error = Some(e);
                    false
                }
            }
        });
        if let Some(e) = error { Err(e) } else { Ok(()) }
    }
    pub fn search_cancellable(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
        cancelled: impl Fn() -> bool,
    ) -> Result<NodeIds, String> {
        let matcher = crate::engine::search::Matcher::new(
            crate::engine::search::Query {
                term: term.into(),
                case,
                regex,
                scope,
            },
            &self.budget,
        )?;
        let mut matches = NodeIds::new(&self.budget);
        self.search_visit(&matcher, &cancelled, &mut |id, matched| {
            if matched {
                if matches.len() == matches.capacity() {
                    matches.reserve_exact(matches.capacity().max(1024))?;
                }
                matches.push(id as u32)?;
            }
            Ok(())
        })?;
        Ok(matches)
    }
    #[allow(
        dead_code,
        reason = "Convenience entry for tests and standalone benchmarks"
    )]
    pub fn visible(&self, expanded: &[bool]) -> Result<NodeIds, String> {
        let mut rows = NodeIds::new(&self.budget);
        self.visible_into(expanded, &mut rows)?;
        Ok(rows)
    }
    pub fn visible_into(&self, expanded: &[bool], rows: &mut NodeIds) -> Result<(), String> {
        if expanded.len() != self.nodes.len() {
            return Err("Expansion state does not match document".into());
        }
        let next = |id: usize| {
            if self.nodes[id].container() && !expanded[id] {
                self.nodes[id].end()
            } else {
                id + 1
            }
        };
        let mut count: usize = 0;
        let mut id = 0;
        while id < self.nodes.len() {
            count += 1;
            id = next(id);
        }
        if rows.capacity() > count.saturating_mul(2).max(4096) {
            rows.release();
        }
        if count > rows.capacity() {
            let capacity = count
                .saturating_add((count / 8).min(65_536))
                .min(self.nodes.len());
            rows.reserve_exact(capacity.saturating_sub(rows.len()))?;
        }
        rows.clear();
        let mut id = 0;
        while id < self.nodes.len() {
            rows.push(id as u32)?;
            id = next(id);
        }
        Ok(())
    }
    fn direct_children(&self, id: usize) -> impl Iterator<Item = usize> + '_ {
        let mut next = id + 1;
        let end = self.nodes[id].end();
        std::iter::from_fn(move || {
            if next >= end {
                return None;
            }
            let id = next;
            next = self.nodes[id].end();
            Some(id)
        })
    }
}
struct SelectedValues<'a> {
    doc: &'a Document,
    ids: &'a [u32],
}
impl serde::Serialize for SelectedValues<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.ids.len()))?;
        for &id in self.ids {
            sequence.serialize_element(self.doc.memory_at(id as usize))?;
        }
        sequence.end()
    }
}
fn walk(value: &Value, visit: &mut impl FnMut(&Value) -> bool) -> bool {
    if !visit(value) {
        return false;
    }
    match value {
        Value::Object(map) => map.values().all(|child| walk(child, visit)),
        Value::Array(array) => array.iter().all(|child| walk(child, visit)),
        _ => true,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    All,
    Keys,
    Values,
}
fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn short(text: &str, length: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(length).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}
// Bound serialization by output bytes, rather than by the size of the whole document.
// Preview-only formatting: serde's string formatter may scan an entire giant
// scalar before its first write. Escape bounded UTF-8 slices instead.
fn write_escaped_preview_string(output: &mut dyn Write, mut text: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    while !text.is_empty() {
        crate::engine::file_source::check_cancelled()?;
        let mut end = text.len().min(4096);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let escaped = serde_json::to_vec(&text[..end]).map_err(io::Error::other)?;
        output.write_all(&escaped[1..escaped.len() - 1])?;
        text = &text[end..];
    }
    output.write_all(b"\"")
}
fn write_memory_preview(output: &mut dyn Write, value: &Value, depth: usize) -> io::Result<()> {
    fn indent(output: &mut dyn Write, depth: usize) -> io::Result<()> {
        output.write_all(&[b' '; 512][..depth * 2])
    }
    crate::engine::file_source::check_cancelled()?;
    match value {
        Value::String(text) => write_escaped_preview_string(output, text),
        Value::Number(number) => output.write_all(number.as_str().as_bytes()),
        Value::Bool(value) => output.write_all(if *value { b"true" } else { b"false" }),
        Value::Null => output.write_all(b"null"),
        Value::Array(array) => {
            output.write_all(b"[")?;
            for (index, value) in array.iter().enumerate() {
                output.write_all(if index == 0 { b"\n" } else { b",\n" })?;
                indent(output, depth + 1)?;
                write_memory_preview(output, value, depth + 1)?;
            }
            if !array.is_empty() {
                output.write_all(b"\n")?;
                indent(output, depth)?;
            }
            output.write_all(b"]")
        }
        Value::Object(object) => {
            output.write_all(b"{")?;
            for (index, (key, value)) in object.iter().enumerate() {
                output.write_all(if index == 0 { b"\n" } else { b",\n" })?;
                indent(output, depth + 1)?;
                write_escaped_preview_string(output, key)?;
                output.write_all(b": ")?;
                write_memory_preview(output, value, depth + 1)?;
            }
            if !object.is_empty() {
                output.write_all(b"\n")?;
                indent(output, depth)?;
            }
            output.write_all(b"}")
        }
    }
}
fn bounded_preview(
    budget: &BudgetManager,
    serialize: impl FnOnce(&mut dyn Write) -> serde_json::Result<()>,
) -> String {
    let _workspace = match budget.reserve(
        Resource::Memory,
        1024 * 1024,
        "preview formatting workspace",
    ) {
        Ok(lease) => lease,
        Err(error) => return format!("Cannot format preview: {error}"),
    };
    struct PreviewWriter {
        bytes: Vec<u8>,
        truncated: bool,
    }
    impl Write for PreviewWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            crate::engine::file_source::check_cancelled()?;
            let remaining = 200_000usize.saturating_sub(self.bytes.len());
            if buf.len() > remaining {
                self.bytes.extend_from_slice(&buf[..remaining]);
                self.truncated = true;
                return Err(io::Error::other("preview limit"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = PreviewWriter {
        bytes: Vec::new(),
        truncated: false,
    };
    let result = serialize(&mut writer);
    if let Err(error) = result
        && !writer.truncated
    {
        return format!("Cannot format preview: {error}");
    }
    let valid_len = match std::str::from_utf8(&writer.bytes) {
        Ok(_) => writer.bytes.len(),
        Err(error) => error.valid_up_to(),
    };
    let mut text = String::from_utf8(writer.bytes[..valid_len].to_vec()).unwrap();
    if writer.truncated {
        text.push_str("\n… Preview truncated. Copy or export for complete JSON.");
    }
    text
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incremental_memory_previews_match_serde_format_and_stop_on_cancellation() {
        let long = "한🙂\n\"\\".repeat(1500);
        let value =
            serde_json::json!({"string":long,"nested":[true,false,null,{},[],{"a":1.25}],"n":123});
        let doc = Document::parse(&value.to_string()).unwrap();
        assert_eq!(
            doc.preview(0),
            serde_json::to_string_pretty(&value).unwrap()
        );
        let refs = doc.query_path("$.*").unwrap();
        let values: Vec<_> = refs.iter().map(|&id| doc.memory_at(id as usize)).collect();
        assert_eq!(
            doc.results_preview(&refs),
            serde_json::to_string_pretty(&values).unwrap()
        );
        struct Stop {
            writes: usize,
            flag: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Write for Stop {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                if self.writes == 3 {
                    self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut output = Stop {
            writes: 0,
            flag: Arc::clone(&flag),
        };
        crate::engine::file_source::with_io_context(
            crate::engine::file_source::Priority::Interactive,
            crate::engine::file_source::Cancellation::Flag(flag),
            || {
                assert!(write_memory_preview(&mut output, &Value::String(long), 0).is_err());
            },
        );
        assert_eq!(output.writes, 3);
    }
    #[test]
    fn display_paths_are_bounded_without_truncating_copy_paths() {
        let key = "한\n".repeat(10_000);
        let encoded = serde_json::to_string(&key).unwrap();
        let doc = Document::parse(&format!("{{{encoded}:1}}")).unwrap();
        let reference = doc.node_ref(1).unwrap();
        let display = doc.display_path_ref(reference).unwrap();
        assert!(display.len() <= 16 * 1024 + 3);
        assert!(display.ends_with('…'));
        assert_eq!(doc.path_ref(reference).unwrap(), format!("$[{encoded}]"));
    }
    #[test]
    fn visible_ids_reuse_capacity_and_release_large_collapsed_arrays() {
        let doc = Document::parse(&serde_json::json!(vec![0; 10_000]).to_string()).unwrap();
        let mut expanded = vec![true; doc.nodes.len()];
        let mut rows = NodeIds::default();
        doc.visible_into(&expanded, &mut rows).unwrap();
        assert_eq!(rows.len(), doc.nodes.len());
        assert_eq!(rows.capacity(), doc.nodes.len());
        let allocation = rows.as_ptr();
        doc.visible_into(&expanded, &mut rows).unwrap();
        assert_eq!(rows.as_ptr(), allocation);
        expanded[0] = false;
        doc.visible_into(&expanded, &mut rows).unwrap();
        assert_eq!(rows, vec![0]);
        assert!(rows.capacity() <= 4096);
    }
    #[test]
    fn compact_match_capacity_is_capped_by_node_count() {
        let doc = Document::parse(&serde_json::json!(vec!["match"; 10_000]).to_string()).unwrap();
        let ids = doc.search(".*", false, true, SearchScope::All).unwrap();
        assert_eq!(ids.len(), doc.nodes.len());
        assert_eq!(ids.capacity(), doc.nodes.len());
        assert_eq!(ids[ids.len() - 1] as usize, doc.nodes.len() - 1);
        assert_eq!(std::mem::size_of_val(&ids[0]), 4);
    }
    #[test]
    fn compact_index_boundaries_and_root_sentinel() {
        assert_eq!(compact(0).unwrap(), 0);
        assert_eq!(compact(u32::MAX as usize).unwrap(), u32::MAX);
        if let Some(too_many) = (u32::MAX as usize).checked_add(1) {
            assert!(
                compact(too_many)
                    .unwrap_err()
                    .contains("32-bit index limit")
            );
        }
        let doc = Document::parse("[{},[],null]").unwrap();
        assert_eq!(doc.nodes[0].parent(), None);
        assert_eq!(doc.nodes[0].end(), doc.nodes.len());
        for id in 1..doc.nodes.len() {
            assert_eq!(doc.nodes[id].parent(), Some(0));
            assert_eq!(doc.nodes[id].end(), id + 1);
        }
        assert_eq!(
            doc.structure_capacity_bytes(),
            doc.nodes.len() * 28 + (doc.nodes.len() - 1) * 4
        );
    }
    #[test]
    fn compact_index_shares_keys_and_preserves_object_and_array_locations() {
        let d =
            Document::parse(r#"{"z":{"b":1,"a":2},"a":{"a":3,"b":4},"rows":[{"b":5},{"b":6}]}"#)
                .unwrap();
        assert_eq!(std::mem::size_of::<Node>(), 28);
        assert_eq!(std::mem::size_of_val(&d.children[0]), 4);
        assert_eq!(d.keys.len(), 4);
        assert_eq!(d.nodes.capacity(), d.nodes.len());
        for id in 0..d.nodes.len() {
            assert_eq!(d.value().unwrap().pointer(&d.pointer(id)), Some(d.at(id)));
            assert_eq!(d.find_path(&d.path(id)).unwrap(), id);
        }
        assert_eq!(
            d.results_json(&d.query_path("$.rows[1,0,1].b").unwrap()),
            "[\n  6,\n  5,\n  6\n]"
        );
    }
    #[test]
    fn pointer_lookup_rejects_invalid_escapes_and_array_indices() {
        let d = Document::parse(r#"{"~2":1,"~":2,"a":[0,1]}"#).unwrap();
        assert_eq!(d.at(d.find_path("/~02").unwrap()), &serde_json::json!(1));
        for path in ["/~2", "/~", "/a/01", "/a/+1", "/a/-1", "/a/2"] {
            assert!(d.query_path(path).unwrap().is_empty(), "{path}");
        }
    }
    #[test]
    fn search_can_stop_before_scanning_the_document() {
        let d =
            Document::parse(r#"{"name":"你好","number":123456789012345678901234567890}"#).unwrap();
        assert!(
            d.search_cancellable("name", false, false, SearchScope::All, || true)
                .is_err()
        );
        assert_eq!(
            d.search("78901234567890", true, false, SearchScope::Values)
                .unwrap(),
            vec![2]
        );
        assert_eq!(
            d.search("你好", false, false, SearchScope::Values).unwrap(),
            vec![1]
        );
    }
    #[test]
    fn search_preserves_array_index_keys_regex_and_case_semantics() {
        let d = Document::parse(r#"[null,123,{"1":"你好","Email":"e"}]"#).unwrap();
        assert_eq!(
            d.search("1", true, false, SearchScope::Keys).unwrap(),
            vec![2, 4]
        );
        assert_eq!(
            d.search("^[0-9]$", true, true, SearchScope::Keys).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            d.search("1", true, false, SearchScope::Values).unwrap(),
            vec![2]
        );
        assert_eq!(
            d.search("email", false, false, SearchScope::All).unwrap(),
            vec![5]
        );
        assert!(
            d.search("email", true, false, SearchScope::All)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            d.search("你好", false, true, SearchScope::Values).unwrap(),
            vec![4]
        );
    }
    #[test]
    fn result_preview_stops_resolving_nodes_after_the_output_limit() {
        let d =
            Document::parse(&serde_json::json!({"large":"x".repeat(250_000)}).to_string()).unwrap();
        // The invalid trailing ID must never be visited after serialization is truncated.
        assert!(
            d.results_preview(&[1, u32::MAX])
                .contains("Preview truncated")
        );
    }
    #[test]
    fn paths_round_trip_special_keys_and_arrays() {
        let d = Document::parse(r#"{"a/b":{"~x":[{"a.b":"中文","quote\"":null}]},"":42}"#).unwrap();
        for id in 0..d.nodes.len() {
            assert_eq!(d.find_path(&d.path(id)).unwrap(), id);
            assert!(d.value().unwrap().pointer(&d.pointer(id)).is_some());
        }
        assert_eq!(d.find_path("/a~1b/~0x/0/a.b").unwrap(), 4);
        assert!(d.find_path("$.missing").is_err());
        for invalid in ["$[01]", "$.a[", "$[?"] {
            assert!(d.query_path(invalid).is_err());
        }
    }
    #[test]
    fn search_checks_full_values_and_scopes() {
        let d = Document::parse(&format!(
            r#"{{"Email":"{}END","nested":{{"email":"yes"}},"n":null}}"#,
            "x".repeat(200)
        ))
        .unwrap();
        assert_eq!(
            d.search("email", false, false, SearchScope::Keys)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            d.search("email", true, false, SearchScope::Keys)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            d.search("END$", true, true, SearchScope::Values).unwrap(),
            vec![1]
        );
        assert!(d.search("[", false, true, SearchScope::All).is_err());
        assert_eq!(
            d.search("null", false, false, SearchScope::Values).unwrap(),
            vec![4]
        );
    }
    #[test]
    fn collapse_skips_whole_subtree() {
        let d = Document::parse(r#"{"a":[1,{"b":2}],"c":3}"#).unwrap();
        let mut expanded = vec![true; d.nodes.len()];
        expanded[1] = false;
        assert_eq!(d.visible(&expanded).unwrap(), vec![0, 1, 5]);
        expanded[0] = false;
        assert_eq!(d.visible(&expanded).unwrap(), vec![0]);
    }
    #[test]
    fn invalid_json_and_precision() {
        assert!(Document::parse("{bad}").err().unwrap().contains("line"));
        let d = Document::parse("\u{feff}{\"n\":123456789012345678901234567890}").unwrap();
        assert_eq!(d.at(1).to_string(), "123456789012345678901234567890");
    }
    #[test]
    fn jsonpath_queries_support_selectors_filters_and_exact_locations() {
        let d = Document::parse(r#"{"resultList":[{"ANS":"a","n":1},{"ANS":"b","n":2},{"ANS":"a","n":3},{"ANS":"韩国","n":4}],"a/b":{"x.y":true}}"#).unwrap();
        let values = |expr: &str| {
            d.query_path(expr)
                .unwrap()
                .iter()
                .map(|&id| d.at(id as usize).clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values("$.resultList[3].ANS"),
            vec![serde_json::json!("韩国")]
        );
        assert_eq!(values("$.resultList[*].ANS").len(), 4);
        assert_eq!(values("$..ANS").len(), 4);
        assert_eq!(
            values("$.resultList[-1].ANS"),
            vec![serde_json::json!("韩国")]
        );
        assert_eq!(
            values("$.resultList[1:3].n"),
            vec![serde_json::json!(2), serde_json::json!(3)]
        );
        assert_eq!(
            values("$.resultList[?(@.n >= 3)].n"),
            vec![serde_json::json!(3), serde_json::json!(4)]
        );
        assert_eq!(values("$['a/b']['x.y']"), vec![serde_json::json!(true)]);
        let ids = d.query_path("$.resultList[0,2,0].ANS").unwrap();
        assert_ne!(ids[0], ids[1]);
        assert_eq!(ids[0], ids[2]);
        assert!(d.query_path("$.nothing").unwrap().is_empty());
        assert!(d.query_path("$[?").is_err());
        assert!(d.find_path("$.resultList[*]").is_err());
    }
    #[test]
    fn previews_are_bounded_by_selection_not_document_size() {
        let text = serde_json::json!({"small":"한국어", "large":"한".repeat(400_000)}).to_string();
        let d = Document::parse(&text).unwrap();
        assert!(d.bytes > 1_000_000);
        assert_eq!(d.preview(d.find_path("$.small").unwrap()), "\"한국어\"");
        let preview = d.preview(d.find_path("$.large").unwrap());
        assert!(preview.contains("Preview truncated"));
        assert!(!preview.contains('\u{fffd}'));
        assert!(preview.len() < 201_000);
        assert_eq!(d.results_preview(&[]), "[]");
    }
}
