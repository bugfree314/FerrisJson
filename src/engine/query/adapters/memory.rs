//! Existing serde_json_path evaluation, with exact tree-node location mapping.
use crate::document::{Document, NodeIds};
use serde_json_path::{JsonPath, PathElement};

pub(crate) fn execute(doc: &Document, query: &JsonPath, cancelled: &dyn Fn() -> bool) -> Result<NodeIds, String> {
    let value = doc.value().ok_or("Memory query requires the memory backend")?;
    // The library has no cancellation hook inside whole-query evaluation.
    let results = query.query_located(value);
    if cancelled() { return Err("Query cancelled".into()); }
    let mut ids = NodeIds::new(doc.budget());
    ids.reserve_exact(results.len())?;
    for (index, node) in results.iter().enumerate() {
        if index % 1024 == 0 && cancelled() { return Err("Query cancelled".into()); }
        let mut id = 0;
        for element in node.location().iter() {
            id = match element {
                PathElement::Name(name) => doc.child_name(id, name),
                PathElement::Index(index) => doc.child_index(id, *index),
            }.ok_or_else(|| "Query result not found in tree".to_string())?;
        }
        ids.push(id as u32)?;
    }
    Ok(ids)
}
