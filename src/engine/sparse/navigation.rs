use super::*;
pub struct RawSelection {
    snapshot: std::sync::Arc<Snapshot>,
    budget: BudgetManager,
    node: Node,
}
impl RawSelection {
    pub fn budget(&self) -> BudgetManager {
        self.budget.clone()
    }
    pub fn write(&self, out: &mut dyn Write, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        let _memory =
            self.budget
                .reserve(Resource::Memory, 64 * 1024, "independent raw output buffer")?;
        let mut bytes = vec![0; 64 * 1024];
        let mut pos = self.node.start;
        while pos < self.node.end {
            check(cancel)?;
            let n = (self.node.end - pos).min(bytes.len() as u64) as usize;
            self.snapshot
                .read_cancellable(pos, &mut bytes[..n], cancel)
                .map_err(|e| e.to_string())?;
            with_io_slot(cancel, || out.write_all(&bytes[..n])).map_err(|e| e.to_string())?;
            pos += n as u64;
        }
        Ok(())
    }
    pub fn copy(&self, cancel: &dyn Fn() -> bool) -> Result<CopiedValue, String> {
        let n = (self.node.end - self.node.start) as usize;
        if n > crate::export::COPY_LIMIT {
            return Err("Value exceeds 8 MiB clipboard limit; use Export".into());
        }
        let lease = self
            .budget
            .reserve(Resource::Memory, n, "independent clipboard payload")?;
        let mut bytes = Vec::with_capacity(n);
        self.write(&mut bytes, cancel)?;
        Ok(CopiedValue {
            text: String::from_utf8(bytes).map_err(|e| e.to_string())?,
            _lease: lease,
        })
    }
}
impl SparseIndex {
    pub fn output_selection(
        &self,
        node: Node,
        cancel: &dyn Fn() -> bool,
    ) -> Result<RawSelection, String> {
        if !self.complete {
            return Err("Pending: complete validation is required for output".into());
        }
        self.validate(node, cancel)?;
        Ok(RawSelection {
            snapshot: self.snapshot.clone(),
            budget: self.budget.clone(),
            node,
        })
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn tree_preview(
        &self,
        node: Node,
        limit: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        self.validate(node, cancel)?;
        if limit > 200_000 {
            return Err("Tree preview limit exceeds 200000 bytes".into());
        }
        let n = (node.end - node.start).min(limit as u64) as usize;
        let _memory = self
            .budget
            .reserve(Resource::Memory, n * 2 + 16, "tree preview decode")?;
        let mut bytes = vec![0; n];
        self.snapshot
            .read_cancellable(node.start, &mut bytes, cancel)
            .map_err(|e| e.to_string())?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if node.end - node.start > n as u64 {
            text.push('…');
        }
        Ok(text)
    }
    pub fn pointer_edges(
        &mut self,
        pointer: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<(u64, Node)>, String> {
        if pointer.len() > 16 * 1024 {
            return Err("Pointer exceeds 16 KiB".into());
        }
        if pointer.is_empty() {
            return Ok(Vec::new());
        }
        if !pointer.starts_with('/') {
            return Err("Expected JSON Pointer".into());
        }
        let _memory = self.budget.reserve(
            Resource::Memory,
            64 * 1024,
            "tree pointer decode and ancestry",
        )?;
        let mut result = Vec::with_capacity(128);
        let mut parent = self.root;
        for raw in pointer[1..].split('/') {
            check(cancel)?;
            if result.len() >= 127 {
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
            let (ordinal, node) = if parent.kind == Kind::Array {
                if key.len() > 1 && key.starts_with('0') {
                    return Err("Invalid array ordinal".into());
                }
                let ordinal = key.parse::<u64>().map_err(|_| "Invalid array ordinal")?;
                if !self.complete && parent.start == self.root.start && ordinal >= parent.count {
                    return Err("Pending: array element is not committed".into());
                }
                (ordinal, self.child(parent, ordinal, cancel)?)
            } else if parent.kind == Kind::Object {
                let hash = hash_bytes(key.as_bytes());
                let mut cursor = self
                    .store
                    .head(directory_hash(parent.container, hash), cancel)?;
                let mut found = None;
                let mut remaining = self.store.bytes() / 104;
                while cursor != NONE {
                    if remaining == 0 {
                        return Err("Object directory cycle".into());
                    }
                    remaining -= 1;
                    let r = self.store.get(cursor, cancel)?;
                    if r[0] == 3
                        && r[8] == parent.container
                        && r[11] == hash
                        && key_matches(&self.snapshot, r[9], r[10], &key, cancel)?
                    {
                        found = Some((r[6], self.field_node(r, cancel)?));
                        break;
                    }
                    cursor = r[7];
                }
                found.ok_or("Object key not found")?
            } else {
                return Err("Pointer descends into scalar".into());
            };
            result.push((ordinal, node));
            parent = node;
        }
        Ok(result)
    }
    pub fn session_id(&self) -> SessionId {
        self.root.session
    }
    pub fn budget(&self) -> &BudgetManager {
        &self.budget
    }
    pub fn full_rows(&self, node: Node, cancel: &dyn Fn() -> bool) -> Result<u64, String> {
        self.validate(node, cancel)?;
        if node.kind.container() {
            Ok(self.store.get(node.container, cancel)?[9])
        } else {
            Ok(1)
        }
    }
    pub(super) fn object_record(
        &self,
        parent: Node,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Record, String> {
        let container = self.store.get(parent.container, cancel)?;
        let entry = self
            .store
            .directory_find(container[10], ordinal, false, cancel)?
            .ok_or("Object child out of range")?;
        if entry[1] != ordinal {
            return Err("Object ordinal not found".into());
        }
        let r = self.store.get(entry[2], cancel)?;
        if r[0] != 3 || r[8] != parent.container || r[6] != ordinal {
            return Err("Invalid object ordinal directory".into());
        }
        Ok(r)
    }
    pub(super) fn array_boundary(
        &self,
        parent: Node,
        key: u64,
        by_weight: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, u64, u64), String> {
        let container = self.store.get(parent.container, cancel)?;
        let entry = self
            .store
            .directory_find(container[10], key, by_weight, cancel)?
            .ok_or("Array boundary missing")?;
        let r = self.store.get(entry[2], cancel)?;
        if r[0] != 2
            || r[8] != parent.container
            || r[4] != entry[1]
            || r[1] != entry[8]
            || r[2] <= parent.start
            || r[2] >= parent.end
            || r[4] >= parent.count
        {
            return Err("Invalid array boundary directory".into());
        }
        Ok((r[2], r[4], r[1]))
    }
    pub fn child_prefix(
        &mut self,
        parent: Node,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        self.validate(parent, cancel)?;
        if ordinal > parent.count {
            return Err("Child prefix out of range".into());
        }
        if ordinal == 0 {
            return Ok(0);
        }
        if ordinal == parent.count {
            return Ok(self.full_rows(parent, cancel)? - 1);
        }
        if parent.kind == Kind::Object {
            let r = self.store.get(parent.container, cancel)?;
            return Ok(self.store.directory_prefix(r[10], ordinal, cancel)?.0);
        }
        if parent.kind != Kind::Array {
            return Err("Prefix requires a container".into());
        }
        let (mut pos, mut index, mut prefix) =
            self.array_boundary(parent, ordinal, false, cancel)?;
        let mut scan = Scanner::new(
            &self.snapshot,
            &mut self.store,
            &self.budget,
            self.root.session,
            cancel,
            false,
        )?;
        while index < ordinal {
            let node = scan.indexed_node(pos)?;
            prefix = prefix
                .checked_add(scan.indexed_weight(node)?)
                .ok_or("Prefix overflow")?;
            pos = scan.next_array_boundary(node, parent, index)?;
            index += 1;
        }
        Ok(prefix)
    }
    pub fn weighted_child(
        &mut self,
        parent: Node,
        rank: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, Node, u64), String> {
        self.validate(parent, cancel)?;
        if rank >= self.full_rows(parent, cancel)? - 1 {
            return Err("Child rank out of range".into());
        }
        if parent.kind == Kind::Object {
            let root = self.store.get(parent.container, cancel)?[10];
            let (r, inside) = self.store.directory_weight(root, rank, cancel)?;
            return Ok((
                r[1],
                self.field_node(self.store.get(r[2], cancel)?, cancel)?,
                inside,
            ));
        }
        let (mut pos, mut index, mut prefix) = self.array_boundary(parent, rank, true, cancel)?;
        let mut scan = Scanner::new(
            &self.snapshot,
            &mut self.store,
            &self.budget,
            self.root.session,
            cancel,
            false,
        )?;
        loop {
            let node = scan.indexed_node(pos)?;
            let weight = scan.indexed_weight(node)?;
            if rank - prefix < weight {
                return Ok((index, node, rank - prefix));
            }
            prefix += weight;
            pos = scan.next_array_boundary(node, parent, index)?;
            index += 1;
        }
    }
    pub fn key_at(
        &self,
        parent: Node,
        ordinal: u64,
        max_bytes: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(String, bool), String> {
        self.validate(parent, cancel)?;
        if parent.kind == Kind::Array {
            return Ok((ordinal.to_string(), false));
        }
        let r = self.object_record(parent, ordinal, cancel)?;
        let _memory = self.budget.reserve(
            Resource::Memory,
            max_bytes.checked_add(4).ok_or("Key limit overflow")?,
            "bounded tree key decode",
        )?;
        let mut reader = Reader::new(&self.snapshot)?.with_cancellation(cancel);
        reader.pos = r[9];
        reader.expect(b'"')?;
        let mut text = String::with_capacity(max_bytes.min(1024));
        while let Some(c) = reader.character()? {
            if text.len() + c.len_utf8() > max_bytes {
                return Ok((text, true));
            }
            text.push(c);
        }
        Ok((text, false))
    }
}

impl Scanner<'_> {
    pub(super) fn indexed_node(&mut self, position: u64) -> Result<Node, String> {
        let cancel = self.cancelled;
        self.reader.pos = position;
        whitespace(&mut self.reader, cancel)?;
        let start = self.reader.pos;
        if matches!(self.reader.peek()?, Some(b'[' | b'{')) {
            let mut cursor = self.store.head(container_hash(start), cancel)?;
            let mut remaining = self.store.bytes() / 104;
            while cursor != NONE {
                if remaining == 0 {
                    return Err("Container directory cycle".into());
                }
                remaining -= 1;
                let r = self.store.get(cursor, cancel)?;
                if r[0] == 1 && r[2] == start {
                    if r[3] == NONE || r[3] > self.snapshot.len {
                        return Err("Container is Pending".into());
                    }
                    return Ok(Node {
                        session: self.session,
                        start,
                        end: r[3],
                        kind: Kind::decode(r[1])?,
                        container: cursor,
                        count: r[4],
                    });
                }
                cursor = r[7];
            }
            return Err("Container directory missing".into());
        }
        self.begin()?.ok_or("Expected scalar".into())
    }
    pub(super) fn next_array_boundary(
        &mut self,
        node: Node,
        parent: Node,
        index: u64,
    ) -> Result<u64, String> {
        self.reader.pos = node.end;
        whitespace(&mut self.reader, self.cancelled)?;
        if index + 1 < parent.count {
            self.reader.expect(b',')?;
        }
        Ok(self.reader.pos)
    }
    pub(super) fn indexed_weight(&self, node: Node) -> Result<u64, String> {
        if node.kind.container() {
            Ok(self.store.get(node.container, self.cancelled)?[9])
        } else {
            Ok(1)
        }
    }
}
