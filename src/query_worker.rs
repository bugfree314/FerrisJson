//! Independent JSONPath task lane; semantics and requests live in engine::query.
use crate::{
    document::{Document, NodeIds},
    task_worker::Work,
};

pub use crate::engine::query::QueryRequest;
use crate::engine::query::QueryEngine;

pub struct QueryOutput {
    pub expression: String,
    pub ids: NodeIds,
    pub preview: String,
}

pub use crate::query_stream_worker::QueryWorker;

// Collected compatibility lane for existing internal callers; the GUI uses the
// streamed worker above. Do not use this adapter to claim bounded query results.
impl Work for QueryRequest {
    type Output = QueryOutput;

    fn input_bytes(&self) -> usize {
        self.expression.capacity()
    }

    fn execute(self, doc: &Document, cancelled: &dyn Fn() -> bool) -> Result<QueryOutput, String> {
        let ids = QueryEngine::execute(doc, &self, cancelled)?;
        if cancelled() {
            return Err("Query cancelled".into());
        }
        let preview = doc.results_preview(&ids);
        Ok(QueryOutput {
            expression: self.expression,
            ids,
            preview,
        })
    }
}
