/// Structural query input. Text-search flags deliberately belong to search::Query.
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
