//! Sparse keeps its existing single-node Pointer subset and error semantics.
use crate::engine::{budget::Resource, sparse::{Kind, Node, SparseIndex}};

pub(crate) fn pointer(index: &mut SparseIndex, pointer: &str, cancelled: &dyn Fn() -> bool) -> Result<Node, String> {
    if pointer.len() > 16 * 1024 {
        return Err("Sparse pointer exceeds 16 KiB command budget".into());
    }
    let _lease = index.budget()
        .reserve(Resource::Memory, 32 * 1024, "sparse pointer decode")?;
    let mut node = index.root();
    if pointer.is_empty() {
        index.validate(node, cancelled)?;
        return Ok(node);
    }
    if !pointer.starts_with('/') {
        return Err("Expected JSON Pointer starting with /".into());
    }
    for segment in pointer[1..].split('/') {
        let mut decoded = String::with_capacity(segment.len());
        let mut chars = segment.chars();
        while let Some(c) = chars.next() {
            if c == '~' {
                decoded.push(match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => return Err("Invalid JSON Pointer escape".into()),
                });
            } else {
                decoded.push(c);
            }
        }
        node = match node.kind() {
            Kind::Object => index.field(node, &decoded, cancelled)?,
            Kind::Array => {
                if decoded.is_empty()
                    || (decoded.len() > 1 && decoded.starts_with('0'))
                    || !decoded.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err("Invalid array index in JSON Pointer".into());
                }
                index.child(
                    node,
                    decoded.parse().map_err(|_| "Array index overflow")?,
                    cancelled,
                )?
            }
            _ => return Err("Pointer descends into scalar".into()),
        };
    }
    Ok(node)
}
