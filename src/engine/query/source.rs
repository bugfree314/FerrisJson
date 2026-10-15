use super::*;
use crate::{
    document::Document,
    engine::tree::{LegacyProvider, Provider},
};
use serde_json::Value;
pub(super) trait Source {
    fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String>;
    fn child(
        &mut self,
        parent: TreeNode,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<TreeNode, String>;
    fn field(
        &mut self,
        parent: TreeNode,
        name: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, TreeNode)>, String>;
    fn scalar(&mut self, node: TreeNode, cancel: &dyn Fn() -> bool) -> Result<Value, String>;
}
struct Buffer<'a> {
    bytes: Vec<u8>,
    cancel: &'a dyn Fn() -> bool,
}
impl std::io::Write for Buffer<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if (self.cancel)() {
            return Err(std::io::Error::other("Query cancelled"));
        }
        if self.bytes.len() + b.len() > 256 * 1024 {
            return Err(std::io::Error::other("Query scalar exceeds 256 KiB"));
        }
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn scalar(
    cancel: &dyn Fn() -> bool,
    write: impl FnOnce(&mut dyn std::io::Write) -> serde_json::Result<()>,
) -> Result<Value, String> {
    let mut buffer = Buffer {
        bytes: Vec::with_capacity(256),
        cancel,
    };
    write(&mut buffer).map_err(|e| e.to_string())?;
    serde_json::from_slice(&buffer.bytes).map_err(|e| e.to_string())
}
struct ArrayCursor {
    parent: u64,
    start: u64,
    page: crate::engine::sparse::Page,
    next: u64,
}
pub(super) struct FromProvider {
    provider: crate::engine::tree::SparseProvider,
    arrays: std::collections::VecDeque<ArrayCursor>,
}
impl Source for FromProvider {
    fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        self.provider.root(cancel)
    }
    fn child(&mut self, p: TreeNode, i: u64, c: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        if p.kind != "array" {
            return self.provider.child(p, i, c);
        }
        let native = p.sparse().ok_or("Invalid query array token")?;
        let previous = self
            .arrays
            .iter()
            .position(|entry| entry.parent == p.reference.address)
            .and_then(|at| self.arrays.remove(at));
        let entry = if let Some(entry) = previous
            .as_ref()
            .filter(|entry| i >= entry.start && i < entry.start + entry.page.nodes().len() as u64)
        {
            let node = entry.page.nodes()[(i - entry.start) as usize];
            let result = TreeNode::from_sparse(node, self.provider.index.full_rows(node, c)?);
            self.arrays.push_back(previous.unwrap());
            return Ok(result);
        } else {
            let start = i / 64 * 64;
            let position = previous
                .as_ref()
                .filter(|entry| start == entry.start + entry.page.nodes().len() as u64)
                .map(|entry| entry.next);
            let (page, next) = self
                .provider
                .index
                .query_array_page(native, start, position, c)?;
            ArrayCursor {
                parent: p.reference.address,
                start,
                page,
                next,
            }
        };
        let node = *entry
            .page
            .nodes()
            .get((i - entry.start) as usize)
            .ok_or("Query array child out of range")?;
        if self.arrays.len() == 128 {
            self.arrays.pop_front();
        }
        self.arrays.push_back(entry);
        Ok(TreeNode::from_sparse(
            node,
            self.provider.index.full_rows(node, c)?,
        ))
    }
    fn field(
        &mut self,
        p: TreeNode,
        n: &str,
        c: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, TreeNode)>, String> {
        self.provider.query_field(p, n, c)
    }
    fn scalar(&mut self, n: TreeNode, c: &dyn Fn() -> bool) -> Result<Value, String> {
        scalar(c, |w| {
            self.provider
                .output(n, c)
                .map_err(|e| serde_json::Error::io(std::io::Error::other(e)))?
                .write(w, c)
        })
    }
}
struct FromDocument<D> {
    doc: D,
    cursors: std::collections::VecDeque<(u64, u64, usize)>,
}
impl<D: std::ops::Deref<Target = Document>> FromDocument<D> {
    fn new(doc: D) -> Self {
        Self {
            doc,
            cursors: std::collections::VecDeque::with_capacity(128),
        }
    }
}
impl<D: std::ops::Deref<Target = Document>> Source for FromDocument<D> {
    fn root(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        if cancel() {
            return Err("Query cancelled".into());
        }
        Ok(LegacyProvider::document_node(&self.doc, 0))
    }
    fn child(&mut self, p: TreeNode, i: u64, c: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        if c() {
            return Err("Query cancelled".into());
        }
        let parent = self.doc.resolve(p.reference)?;
        if i >= p.children {
            return Err("Child not found".into());
        }
        let id = if p.kind == "array" {
            self.doc
                .child_index(parent, i as usize)
                .ok_or("Child not found")?
        } else {
            let cursor = self
                .cursors
                .iter()
                .position(|(p0, _, _)| *p0 == p.reference.address)
                .and_then(|at| self.cursors.remove(at));
            let (mut ordinal, mut id) = cursor
                .filter(|(_, o, _)| *o <= i)
                .map(|(_, o, id)| (o, id))
                .unwrap_or((0, parent + 1));
            while ordinal < i {
                if c() {
                    return Err("Query cancelled".into());
                }
                id = self.doc.node_info(id).subtree_end;
                ordinal += 1;
            }
            if self.cursors.len() == 128 {
                self.cursors.pop_front();
            }
            self.cursors.push_back((p.reference.address, i, id));
            id
        };
        Ok(LegacyProvider::document_node(&self.doc, id))
    }

    fn field(
        &mut self,
        p: TreeNode,
        name: &str,
        c: &dyn Fn() -> bool,
    ) -> Result<Option<(u64, TreeNode)>, String> {
        if p.kind != "object" {
            return Ok(None);
        }
        let Some(id) = self.doc.child_name(p.reference.address as usize, name) else {
            return Ok(None);
        };
        let (mut lo, mut hi) = (0, p.children);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.child(p, mid, c)?.reference.address < id as u64 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(Some((lo, LegacyProvider::document_node(&self.doc, id))))
    }
    fn scalar(&mut self, n: TreeNode, c: &dyn Fn() -> bool) -> Result<Value, String> {
        scalar(c, |w| self.doc.write_node(w, n.reference.address as usize))
    }
}
/// Compatibility collectors are bounded independently of total intermediate cardinality.
pub fn collect(
    doc: &Document,
    plan: &Plan,
    cancel: &dyn Fn() -> bool,
) -> Result<crate::engine::ids::NodeIds, String> {
    let _workspace = doc.budget().reserve(
        Resource::Memory,
        16 * 1024 * 1024,
        "bounded compatibility query workspace",
    )?;
    let mut source = FromDocument::new(doc);
    let root = Location {
        node: LegacyProvider::document_node(doc, 0),
        ancestors: Vec::new(),
    };
    let mut runner = Runner {
        source: &mut source,
        root: root.clone(),
        cancel,
        depth: 0,
        visited: 0,
    };
    let mut ids = crate::engine::ids::NodeIds::new(doc.budget());
    runner.eval(plan, root, 0, &mut |loc, _| {
        if let Some(loc) = loc {
            if ids.len() >= 65_536 {
                return Err(
                    "Compatibility query is limited to 65536 results; use --paged-tree or --sparse"
                        .into(),
                );
            }
            ids.push(
                u32::try_from(loc.node.reference.address)
                    .map_err(|_| "Legacy result address overflow")?,
            )?;
        }
        Ok(true)
    })?;
    Ok(ids)
}

pub(super) fn input(input: Input) -> Result<Box<dyn Source>, String> {
    match input {
        Input::Legacy(doc) => Ok(Box::new(FromDocument::new(doc))),
        Input::Sparse(index) => {
            if !index.complete() {
                return Err("Query requires full source validation".into());
            }
            Ok(Box::new(FromProvider {
                provider: crate::engine::tree::SparseProvider::new(*index)?,
                arrays: std::collections::VecDeque::with_capacity(128),
            }))
        }
    }
}
