use super::{Backend, QueryEngine, adapters};
use crate::{
    document::{Document, NodeIds},
    engine::{budget::{Reservation, Resource}, session::SessionId},
};
use serde_json_path::JsonPath;

enum Body {
    Pointer,
    Memory(JsonPath),
    Indexed(adapters::indexed::Plan),
}

/// A compiled compatibility query bound to its source session and backend.
/// Library AST allocations and the legacy Indexed parser are not fully ledgered.
/// This collected compatibility API is separate from QueryEngine::run.
pub struct CompiledQuery {
    normalized: String,
    body: Body,
    backend: Backend,
    session: SessionId,
    _source: Reservation,
}

impl CompiledQuery {
    pub fn compile(doc: &Document, expression: &str, cancelled: &dyn Fn() -> bool) -> Result<Self, String> {
        if cancelled() { return Err("Query cancelled".into()); }
        let expression = expression.trim();
        let pointer = expression.starts_with('/');
        let prefix = !pointer && (expression.starts_with('.') || expression.starts_with('['));
        let source_len = expression.len().checked_add(usize::from(prefix)).ok_or("Query source length overflow")?;
        let mut source = doc.budget().reserve(Resource::Memory, source_len, "normalized query source")?;
        let mut normalized = String::new();
        normalized.try_reserve_exact(source_len).map_err(|e| format!("Cannot allocate query source: {e}"))?;
        if expression == "." { normalized.push('$'); }
        else {
            if prefix { normalized.push('$'); }
            normalized.push_str(expression);
        }
        source.resize(normalized.capacity())?;
        let backend = QueryEngine::backend(doc);
        let body = if pointer {
            Body::Pointer
        } else {
            // Preserve the common library syntax validation before Indexed capability checks.
            let parsed = JsonPath::parse(&normalized).map_err(|e| format!("Invalid JSONPath: {e}"))?;
            if cancelled() { return Err("Query cancelled".into()); }
            match backend {
                Backend::Memory => Body::Memory(parsed),
                Backend::Indexed => {
                    drop(parsed);
                    Body::Indexed(adapters::indexed::compile(&normalized, cancelled)?)
                }
                Backend::Sparse => return Err("Sparse general JSONPath is not supported".into()),
            }
        };
        Ok(Self { normalized, body, backend, session: doc.session_id(), _source: source })
    }

    pub fn normalized(&self) -> &str { &self.normalized }
    pub fn backend(&self) -> Backend { self.backend }

    pub fn execute(&self, doc: &Document, cancelled: &dyn Fn() -> bool) -> Result<NodeIds, String> {
        if cancelled() { return Err("Query cancelled".into()); }
        if doc.session_id() != self.session || QueryEngine::backend(doc) != self.backend {
            return Err("Compiled query belongs to another document session or backend".into());
        }
        let ids = match &self.body {
            Body::Pointer => {
                let mut ids = NodeIds::new(doc.budget());
                if let Some(id) = adapters::pointer::locate(doc, &self.normalized, cancelled)? { ids.push(id as u32)?; }
                ids
            }
            Body::Memory(query) => adapters::memory::execute(doc, query, cancelled)?,
            Body::Indexed(plan) => adapters::indexed::execute(doc, plan, cancelled)?,
        };
        if cancelled() { return Err("Query cancelled".into()); }
        Ok(ids)
    }
}
