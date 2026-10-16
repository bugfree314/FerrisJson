//! Independent JSONPath lane. Query evaluation remains a legacy adapter in B.
use crate::{
    document::{Document, NodeIds},
    task_worker::{TaskWorker, Work},
};

pub struct QueryRequest {
    pub expression: String,
}

impl QueryRequest {
    pub fn new(expression: impl Into<String>) -> Self {
        Self {
            expression: expression.into(),
        }
    }
}

pub struct QueryOutput {
    pub expression: String,
    pub ids: NodeIds,
    pub preview: String,
}

pub type QueryWorker = TaskWorker<QueryRequest>;

impl Work for QueryRequest {
    type Output = QueryOutput;

    fn input_bytes(&self) -> usize {
        self.expression.capacity()
    }

    fn execute(self, doc: &Document, cancelled: &dyn Fn() -> bool) -> Result<QueryOutput, String> {
        // The memory library still evaluates a whole query without a cancellation hook.
        let ids = doc.query_path_cancellable(&self.expression, cancelled)?;
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
