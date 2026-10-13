//! Virtual visible rows. Only explicit expansion/collapse overrides are stored.
use super::{Document, Node};
use std::collections::{HashMap, HashSet};
const LIMIT: usize = 16_384;
pub struct LazyView {
    all: bool,
    open: HashSet<u32>,
    closed: HashSet<u32>,
    counts: HashMap<u32, usize>,
    children: HashMap<u32, Vec<(u32, u32)>>,
    hidden: Vec<(usize, usize)>,
    len: usize,
}
impl LazyView {
    pub fn new(doc: &Document) -> Result<Self, String> {
        let mut result = Self {
            all: false,
            open: HashSet::from([0]),
            closed: HashSet::new(),
            counts: HashMap::new(),
            children: HashMap::new(),
            hidden: Vec::new(),
            len: 1,
        };
        result.rebuild(doc)?;
        Ok(result)
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_open(&self, id: usize) -> bool {
        if self.all {
            !self.closed.contains(&(id as u32))
        } else {
            self.open.contains(&(id as u32))
        }
    }
    pub fn set_open(&mut self, id: usize, open: bool) -> Result<(), String> {
        let id = id as u32;
        let set = if self.all {
            &mut self.closed
        } else {
            &mut self.open
        };
        let insert = if self.all { !open } else { open };
        if insert {
            if !set.contains(&id) && set.len() >= LIMIT {
                return Err(
                    "Expansion overrides exceed 16,384-entry memory budget; collapse some branches"
                        .into(),
                );
            }
            set.insert(id);
        } else {
            set.remove(&id);
        }
        Ok(())
    }
    pub fn all(&mut self, open: bool) {
        self.all = open;
        self.open.clear();
        self.closed.clear();
    }
    pub fn rebuild(&mut self, doc: &Document) -> Result<(), String> {
        self.counts.clear();
        self.children.clear();
        self.hidden.clear();
        if self.all {
            let mut spans = self
                .closed
                .iter()
                .map(|id| doc.node(*id as usize).map(|n| (*id as usize + 1, n.end())))
                .collect::<Result<Vec<_>, _>>()?;
            spans.sort_unstable();
            for (start, end) in spans {
                if end <= start
                    || self
                        .hidden
                        .last()
                        .is_some_and(|(_, previous)| start < *previous)
                {
                    continue;
                }
                self.hidden.push((start, end));
            }
            self.len = doc.node_count()
                - self
                    .hidden
                    .iter()
                    .map(|(start, end)| end - start)
                    .sum::<usize>();
            return Ok(());
        }
        let mut nodes: Vec<(u32, Node)> = self
            .open
            .iter()
            .map(|id| doc.node(*id as usize).map(|n| (*id, n)))
            .collect::<Result<_, _>>()?;
        nodes.sort_unstable_by_key(|(_, n)| std::cmp::Reverse(n.depth));
        for (id, node) in &nodes {
            let count = 1
                + node.child_count as usize
                + self.children.get(id).map_or(0, |children| {
                    children
                        .iter()
                        .map(|(_, child)| self.counts[child] - 1)
                        .sum::<usize>()
                });
            self.counts.insert(*id, count);
            if let Some(parent) = node.parent()
                && self.open.contains(&(parent as u32))
            {
                self.children
                    .entry(parent as u32)
                    .or_default()
                    .push((node.segment, *id));
            }
        }
        for children in self.children.values_mut() {
            children.sort_unstable();
        }
        self.len = self.counts.get(&0).copied().unwrap_or(1);
        Ok(())
    }
    pub fn row(&self, doc: &Document, mut row: usize) -> Result<u32, String> {
        if row >= self.len {
            return Err("Visible row out of bounds".into());
        }
        if self.all {
            let mut id = row;
            for (start, end) in &self.hidden {
                if id < *start {
                    break;
                }
                id += end - start;
            }
            return Ok(id as u32);
        }
        let mut parent = 0u32;
        loop {
            if row == 0 {
                return Ok(parent);
            }
            row -= 1;
            let mut ordinal = 0;
            let mut descend = None;
            if let Some(children) = self.children.get(&parent) {
                for (expanded_ordinal, child) in children {
                    let gap = *expanded_ordinal as usize - ordinal;
                    if row < gap {
                        return doc
                            .child_ordinal(parent as usize, ordinal + row)
                            .map(|id| id as u32);
                    }
                    row -= gap;
                    let count = self.counts[child];
                    if row < count {
                        descend = Some(*child);
                        break;
                    }
                    row -= count;
                    ordinal = *expanded_ordinal as usize + 1;
                }
            }
            if let Some(child) = descend {
                parent = child;
            } else {
                return doc
                    .child_ordinal(parent as usize, ordinal + row)
                    .map(|id| id as u32);
            }
        }
    }
    pub fn position(&self, doc: &Document, id: usize) -> Result<Option<usize>, String> {
        if self.all {
            let mut row = id;
            for (start, end) in &self.hidden {
                if id < *start {
                    break;
                }
                if id < *end {
                    return Ok(None);
                }
                row -= end - start;
            }
            return Ok(Some(row));
        }
        let mut chain = Vec::new();
        let mut child = id;
        while let Some(parent) = doc.node(child)?.parent() {
            chain.push((parent, doc.node(child)?.segment));
            child = parent;
        }
        let mut row = 0;
        for (parent, ordinal) in chain.into_iter().rev() {
            if !self.is_open(parent) {
                return Ok(None);
            }
            row += 1 + ordinal as usize;
            if let Some(children) = self.children.get(&(parent as u32)) {
                for (_, child) in children.iter().take_while(|(n, _)| *n < ordinal) {
                    row += self.counts[child] - 1;
                }
            }
        }
        Ok(Some(row))
    }
}
