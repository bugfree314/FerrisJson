//! Independent JSONPath task lane; semantics and requests live in engine::query.
use crate::document::NodeIds;
#[cfg(test)]
use crate::{document::Document, task_worker::Work};

pub use crate::engine::query::QueryRequest;

pub struct QueryOutput {
    pub expression: String,
    pub ids: NodeIds,
    pub preview: String,
}

pub use crate::query_stream_worker::QueryWorker;

// Preserve the scheduler's existing fixture adapter only in test builds.
// Production callers submit the streamed worker, never this collected job.
#[cfg(test)]
impl Work for QueryRequest {
    type Output = QueryOutput;

    fn input_bytes(&self) -> usize {
        self.expression.capacity()
    }

    fn execute(self, doc: &Document, cancelled: &dyn Fn() -> bool) -> Result<QueryOutput, String> {
        let ids = crate::engine::query::compatibility::collect(doc, &self.expression, cancelled)?;
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
