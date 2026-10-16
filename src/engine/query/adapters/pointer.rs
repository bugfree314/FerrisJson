//! Legacy Pointer semantics: invalid or missing paths select no nodes.
use crate::document::Document;

pub(crate) fn locate(doc: &Document, pointer: &str, cancelled: &dyn Fn() -> bool) -> Result<Option<usize>, String> {
    let mut id = 0;
    let Some(path) = pointer.strip_prefix('/') else { return Ok(None); };
    for token in path.split('/') {
        if cancelled() { return Err("Query cancelled".into()); }
        let mut bytes = token.bytes();
        while let Some(byte) = bytes.next() {
            if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
                return Ok(None);
            }
        }
        let token = token.replace("~1", "/").replace("~0", "~");
        id = if doc.node_info(id).kind() == "array" {
            if token.starts_with('+') || (token.len() > 1 && token.starts_with('0')) {
                return Ok(None);
            }
            match token.parse().ok().and_then(|index| doc.child_index(id, index)) { Some(child) => child, None => return Ok(None) }
        } else {
            match doc.child_name(id, &token) { Some(child) => child, None => return Ok(None) }
        };
    }
    Ok(Some(id))
}
